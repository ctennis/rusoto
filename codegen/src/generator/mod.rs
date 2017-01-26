use std::fs::File;
use std::io::{Write, BufWriter};
use std::path::Path;

use inflector::Inflector;

use botocore::{Service, Shape, ShapeType};
use self::json::JsonGenerator;
use self::query::QueryGenerator;
use self::rest_json::RestJsonGenerator;
use self::rest_xml::RestXmlGenerator;
use self::error_types::{GenerateErrorTypes, JsonErrorTypes, XmlErrorTypes};
use self::tests::generate_tests;
use self::type_filter::filter_types;

mod error_types;
mod json;
mod query;
mod rest_json;
mod tests;
mod rest_xml;
mod xml_payload_parser;
mod rest_response_parser;
mod type_filter;

type FileWriter = BufWriter<File>;
type IoResult = ::std::io::Result<()>;

/// Abstracts the generation of Rust code for various AWS protocols
pub trait GenerateProtocol {

    /// Generate the various `use` statements required by the module generatedfor this service
    fn generate_prelude(&self, writer: &mut FileWriter, service: &Service) -> IoResult;

    /// Generate a method for each `Operation` in the `Service` to execute that method remotely
    ///
    /// The method generated by this method are inserted into an enclosing `impl FooClient {}` block
    fn generate_methods(&self, writer: &mut FileWriter, service: &Service) -> IoResult;

    /// Add any attributes that should decorate the struct for the given type (typically `Debug`, `Clone`, etc.)
    fn generate_struct_attributes(&self,
                                  struct_name: &str,
                                  serialized: bool,
                                  deserialized: bool)
                                  -> String;

    /// If necessary, generate a serializer for the specified type
    fn generate_serializer(&self, _name: &str, _shape: &Shape, _service: &Service) -> Option<String> {
        None
    }

    /// If necessary, generate a deserializer for the specified type
    fn generate_deserializer(&self, _name: &str, _shape: &Shape, _service: &Service) -> Option<String> {
        None
    }

    /// Return the type used by this protocol for timestamps
    fn timestamp_type(&self) -> &'static str;
}

/// Given a botocore `Service` object, determine its protocol and use the appropriate
/// code generator to write code directly to a file at the specified path
///
/// # Panics
/// If the service metadata indicates a protocol for which a code generator does not exist
pub fn generate_source(service: &Service, output_path: &Path) -> IoResult {
    let output_file = File::create(output_path).expect(&format!(
        "Couldn't open file for writing: {:?}",
        output_path,
    ));

    let mut writer = BufWriter::new(output_file);

    // EC2 service protocol is similar to query but not the same.  Rusoto is able to generate Rust code
    // from the service definition through the same QueryGenerator, but botocore uses a special class.
    // See https://github.com/boto/botocore/blob/dff99fdf2666accf6b448aef7f03fe3d66dd38fa/botocore/serialize.py#L259-L266 .
    match &service.metadata.protocol[..] {
        "json" => generate(&mut writer, service, JsonGenerator, JsonErrorTypes),
        "query" | "ec2" => generate(&mut writer, service, QueryGenerator, XmlErrorTypes),
        "rest-json" => generate(&mut writer, service, RestJsonGenerator, JsonErrorTypes),
        "rest-xml" => generate(&mut writer, service, RestXmlGenerator, XmlErrorTypes),
        protocol => panic!("Unknown protocol {}", protocol),
    }
}

/// Translate a botocore field name to something rust-idiomatic and
/// escape reserved words with an underscore
pub fn generate_field_name(member_name: &str) -> String {
    let name = member_name.to_snake_case();
    if name == "return" || name == "type" {
        name + "_"
    } else {
        name
    }
}

/// Capitalize the first character in the given string.
/// If the input string is empty an empty string is returned.
pub fn capitalize_first<S>(word: S) -> String
    where S: Into<String> {
    let s = word.into();
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The quick brown fox jumps over the lazy dog
fn generate<P, E>(writer: &mut FileWriter, service: &Service, protocol_generator: P, error_type_generator: E) -> IoResult
    where P: GenerateProtocol,
          E: GenerateErrorTypes {

    writeln!(writer, "#[allow(warnings)]
        use hyper::Client;
        use hyper::status::StatusCode;
        use request::DispatchSignedRequest;
        use region;

        use std::fmt;
        use std::error::Error;
        use request::HttpDispatchError;
        use rusoto_credential::{{CredentialsError, ProvideAwsCredentials}};
    ")?;

    protocol_generator.generate_prelude(writer, service)?;
    generate_types(writer, service, &protocol_generator)?;
    error_type_generator.generate_error_types(writer, service)?;
    generate_client(writer, service, &protocol_generator)?;
    generate_tests(writer, service)?;

    Ok(())

}

fn generate_client<P>(writer: &mut FileWriter, service: &Service, protocol_generator: &P) -> IoResult
    where P: GenerateProtocol {
    // If the struct name is changed, the links in each service documentation should change.
    // See https://github.com/rusoto/rusoto/issues/519
    writeln!(writer,
        "/// A client for the {service_name} API.
        pub struct {type_name}<P, D> where P: ProvideAwsCredentials, D: DispatchSignedRequest {{
            credentials_provider: P,
            region: region::Region,
            dispatcher: D,
        }}

        impl<P, D> {type_name}<P, D> where P: ProvideAwsCredentials, D: DispatchSignedRequest {{
            pub fn new(request_dispatcher: D, credentials_provider: P, region: region::Region) -> Self {{
                  {type_name} {{
                    credentials_provider: credentials_provider,
                    region: region,
                    dispatcher: request_dispatcher
                }}
            }}

        ",
        service_name = match &service.metadata.service_abbreviation {
            &Some(ref service_abbreviation) => service_abbreviation.as_str(),
            &None => service.metadata.service_full_name.as_ref()
        },
        type_name = service.client_type_name(),
    )?;
    protocol_generator.generate_methods(writer, service)?;
    writeln!(writer, "}}")
}

fn generate_list(name: &str, shape: &Shape) -> String {
    format!("pub type {} = Vec<{}>;",
            name,
            mutate_type_name(shape.member_type()))
}

fn generate_map(name: &str, shape: &Shape) -> String {
    format!(
        "pub type {} = ::std::collections::HashMap<{}, {}>;",
        name,
        capitalize_first(shape.key_type().to_string()),
        capitalize_first(shape.value_type().to_string()),
    )
}

fn generate_primitive_type(name: &str, shape_type: ShapeType, for_timestamps: &str) -> String {
    let primitive_type = match shape_type {
        ShapeType::Blob => "Vec<u8>",
        ShapeType::Boolean => "bool",
        ShapeType::Double => "f64",
        ShapeType::Float => "f32",
        ShapeType::Integer => "i32",
        ShapeType::Long => "i64",
        ShapeType::String => "String",
        ShapeType::Timestamp => for_timestamps,
        primitive_type => panic!("Unknown primitive type: {:?}", primitive_type),
    };

    format!("pub type {} = {};", name, primitive_type)
}

// do any type name mutation needed to avoid collisions with Rust types
fn mutate_type_name(type_name: &str) -> String {
    let capitalized = capitalize_first(type_name.to_owned());

    // some cloudfront types have underscoare that anger the lint checker
    let without_underscores = capitalized.replace("_", "");

    match &without_underscores[..] {
        // S3 has an 'Error' shape that collides with Rust's Error trait
        "Error" => "S3Error".to_string(),

        // EC2 has a CancelSpotFleetRequestsError struct, avoid collision with our error enum
        "CancelSpotFleetRequests" => "EC2CancelSpotFleetRequests".to_owned(),

        // otherwise make sure it's rust-idiomatic and capitalized
        _ => without_underscores,
    }
}

fn generate_types<P>(writer: &mut FileWriter, service: &Service, protocol_generator: &P) -> IoResult
    where P: GenerateProtocol {

    let (serialized_types, deserialized_types) = filter_types(service);

    for (name, shape) in &service.shapes {
            let type_name = mutate_type_name(&name);

            // We generate enums for error types, so no need to create model objects for them
            if shape.exception() {
                continue;
            }

            // If botocore includes documentation, clean it up a bit and use it
            if let Some(ref docs) = shape.documentation {
                writeln!(writer, "#[doc=\"{}\"]",
                                   docs.replace("\\", "\\\\").replace("\"", "\\\""))?;
            }

            let deserialized = deserialized_types.contains(&type_name);
            let serialized = serialized_types.contains(&type_name);

            // generate a rust type for the shape
            if type_name != "String" {
                let generated_type = match shape.shape_type {
                    ShapeType::Structure => {
                        generate_struct(service,
                                                   &type_name,
                                                   &shape,
                                                   serialized,
                                                   deserialized,
                                                   protocol_generator)
                    }
                    ShapeType::Map => generate_map(&type_name, &shape),
                    ShapeType::List => generate_list(&type_name, &shape),
                    shape_type => {
                        generate_primitive_type(&type_name,
                                                           shape_type,
                                                           protocol_generator.timestamp_type())
                    }
                };
                writeln!(writer, "{}", generated_type)?;
            }

            if deserialized {
                if let Some(deserializer) = protocol_generator.generate_deserializer(&type_name, &shape, service) {
                    writeln!(writer, "{}", deserializer)?;
                }
            }

            if serialized {
                if let Some(serializer) = protocol_generator.generate_serializer(&type_name, &shape, service) {
                    writeln!(writer, "{}", serializer)?;
                }
            }
        }
        Ok(())
}



fn generate_struct<P>(service: &Service,
                      name: &str,
                      shape: &Shape,
                      serialized: bool,
                      deserialized: bool,
                      protocol_generator: &P)
                      -> String
    where P: GenerateProtocol {

    if shape.members.is_none() || shape.members.as_ref().unwrap().is_empty() {
        format!(
            "{attributes}
            pub struct {name};
            ",
            attributes = protocol_generator.generate_struct_attributes(name, serialized, deserialized),
            name = name,
        )
    } else {
        let struct_attributes =
            protocol_generator.generate_struct_attributes(name, serialized, deserialized);
        // Serde attributes are only needed if deriving the Serialize or Deserialize trait
        let need_serde_attrs = struct_attributes.contains("erialize");
        format!(
            "{attributes}
            pub struct {name} {{
                {struct_fields}
            }}
            ",
            attributes = struct_attributes,
            name = name,
            struct_fields = generate_struct_fields(service, shape, need_serde_attrs),
        )
    }

}

fn generate_struct_fields(service: &Service, shape: &Shape, serde_attrs: bool) -> String {
    shape.members.as_ref().unwrap().iter().filter_map(|(member_name, member)| {

        if member.deprecated == Some(true) {
            return None;
        }

        let mut lines: Vec<String> = Vec::new();
        let name = generate_field_name(member_name);

        if let Some(ref docs) = member.documentation {
            lines.push(format!("#[doc=\"{}\"]", docs.replace("\\","\\\\").replace("\"", "\\\"")));
        }

        let type_name = mutate_type_name(&member.shape);

        if serde_attrs {
            lines.push(format!("#[serde(rename=\"{}\")]", member_name));

            if let Some(shape_type) = service.shape_type_for_member(member) {
                if shape_type == ShapeType::Blob {
                    lines.push(
                        "#[serde(
                            deserialize_with=\"::serialization::SerdeBlob::deserialize_blob\",
                            serialize_with=\"::serialization::SerdeBlob::serialize_blob\",
                            default,
                        )]".to_owned()
                    );
                } else if shape_type == ShapeType::Boolean && !shape.required(member_name) {
                    lines.push("#[serde(skip_serializing_if=\"::std::option::Option::is_none\")]".to_owned());
                }
            }
        }

        if shape.required(member_name) {
            lines.push(format!("pub {}: {},",  name, type_name))
        } else if name == "type" {
            lines.push(format!("pub aws_{}: Option<{}>,",  name, type_name))
        } else {
            lines.push(format!("pub {}: Option<{}>,",  name, type_name))
        }

        Some(lines.join("\n"))
    }).collect::<Vec<String>>().join("\n")
}

fn error_type_name(name: &str) -> String {
    let type_name = mutate_type_name(name);
    format!("{}Error", type_name)
}

#[test]
fn capitalize_first_test() {
    assert_eq!(capitalize_first("a &str test"), "A &str test".to_owned());
    assert_eq!(capitalize_first("a String test".to_owned()),
               "A String test".to_owned());
}
