use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum SourceFormat {
    /// JSON Format
    #[default]
    Json,
    /// Apache access and error log lines
    Apache(ApacheLogConfig),
    /// Elastic Load Balancer access log lines
    AwsAlb,
    /// Amazon CloudWatch logs
    AwsCloudwatchSubscriptionMessage,
    /// VPC Flow logs format
    AwsVpcFlow(AwsVpcFlowConfig),
    /// CEF (Common Event Format)
    Cef(CefConfig),
    /// CLF (Common Log Format)
    CLF(ClfConfig),
    /// CSV (Comma Separated Values)
    CSV(CsvConfig),
    /// GLOG (Google Logging Library)
    Glog,
    /// Grok format
    Grok(GrokConfig),
    /// Key value format
    KeyValue(KeyValueConfig),
    /// Klog format
    Klog,
    /// Linux authorization logs
    LinuxAuthorization,
    /// Logfmt format
    Logfmt,
    /// Nginx access and error log lines
    Nginx(NginxLogConfig),
    /// Syslog format
    Syslog,
    /// Xml format
    Xml(XmlConfig),
    /// Raw format. Using this format parses the incoming message into JSON object with a single
    /// key `message` containing the raw message.
    ///
    /// e.g. `{"message": "hello my unmodified log message"}`
    Raw,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ApacheLogConfig {
    /// The format of the Apache log.
    #[schema(value_type = String)]
    pub format: ApacheLogFormat,
    /// The date/time format to use for encoding the timestamp. The time is parsed in local time if
    /// the timestamp doesn’t specify a timezone. Defaults to `%d/%b/%Y:%T %z`.
    pub timestamp_format: Option<String>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum ApacheLogFormat {
    /// Apache combined log format.
    Combined,
    /// Apache common log format.
    Common,
    /// Apache error log format.
    #[default]
    Error,
}

impl ToString for ApacheLogFormat {
    fn to_string(&self) -> String {
        match self {
            ApacheLogFormat::Combined => "combined".to_string(),
            ApacheLogFormat::Common => "common".to_string(),
            ApacheLogFormat::Error => "error".to_string(),
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AwsVpcFlowConfig {
    /// The format of the VPC Flow log.
    pub format: Option<String>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CefConfig {
    /// Toggles translation of custom field pairs to key: value.
    pub translate_custom_fields: Option<bool>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClfConfig {
    /// The date/time format to use for encoding the timestamp.
    /// Defaults to `%d/%b/%Y:%T %z`
    pub timestamp_format: Option<String>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CsvConfig {
    /// The field delimiter to use when parsing. Must be a single-byte utf8 character.
    /// Defaults to `,`.
    pub delimiter: Option<u8>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrokConfig {
    /// The grok pattern to use when parsing.
    pub pattern: String,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KeyValueConfig {
    /// The string that separates the key from the value. Defaults to `=`.
    pub key_value_delimiter: Option<String>,
    /// The string that separates each key/value pair.
    pub field_delimeter: Option<String>,
    /// Defines the acceptance of unnecessary whitespace surrounding the configured
    /// `key_value_delimiter`. Possible values are `strict` and `lenient`.
    /// Defaults to `lenient`.
    pub whitespace: Option<String>,
    /// Whether a standalone key should be accepted, the resulting object will associate such keys
    /// with boolean value `true`. Defaults to `true`.
    pub accept_standalone_key: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NginxLogConfig {
    /// The format to use for parsing the log.
    pub format: NginxLogFormat,
    /// The date/time format to use for encoding the timestamp. The time is parsed in local time if
    /// the timestamp doesn’t specify a timezone. The default format is `%d/%b/%Y:%T %z` for
    /// combined logs and `%Y/%m/%d %H:%M:%S` for error logs.
    pub timestamp_format: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum NginxLogFormat {
    /// Nginx common log format.
    Combined,
    /// Nginx error log format.
    Error,
}

impl ToString for NginxLogFormat {
    fn to_string(&self) -> String {
        match self {
            NginxLogFormat::Combined => "combined".to_string(),
            NginxLogFormat::Error => "error".to_string(),
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct XmlConfig {
    /// Whether to include XML tag attributes in the returned object. Defaults to `true`.
    pub include_attr: Option<bool>,
    /// String prefix to use for XML tag attribute keys. Defaults to `@`.
    pub attr_prefix: Option<String>,
    /// Key name to use for expanded text nodes. Defaults to `text`.
    pub text_key: Option<String>,
    /// Whether to always return text nodes as {"<text_key>": "value"}. Defaults to `false`.
    pub always_use_text_key: Option<bool>,
    /// Whether to parse “true” and “false” as boolean. Defaults to `true`.
    pub parse_bool: Option<bool>,
    /// Whether to parse “null” as null. Defaults to `true`.
    pub parse_null: Option<bool>,
    /// Parse numbers as integers/floats. Defaults to `true`.
    pub parse_number: Option<bool>,
}

pub trait IntoVrlScript {
    fn into_vrl_script(self) -> String;
}

impl IntoVrlScript for SourceFormat {
    fn into_vrl_script(self) -> String {
        match self {
            SourceFormat::Json => "".to_string(),
            SourceFormat::AwsAlb => VrlScriptFunction::new("parse_aws_alb_log").build(),
            SourceFormat::AwsCloudwatchSubscriptionMessage => {
                VrlScriptFunction::new("parse_aws_cloudwatch_subscription_message")
                    .build()
            },
            SourceFormat::Klog => VrlScriptFunction::new("parse_klog").build(),
            SourceFormat::LinuxAuthorization => {
                VrlScriptFunction::new("parse_linux_authorization").build()
            },
            SourceFormat::Logfmt => VrlScriptFunction::new("parse_logfmt").build(),
            SourceFormat::Syslog => VrlScriptFunction::new("parse_syslog").build(),
            SourceFormat::Raw => "".to_string(),

        }
    }
}


impl IntoVrlScript for ApacheLogConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_apache_log")
            .add_arg("format", &self.format.to_string())
            .add_optional_arg("timestamp_format", self.timestamp_format.as_deref())
            .build()
    }
}

impl IntoVrlScript for AwsVpcFlowConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_aws_vpc_flow_log")
            .add_optional_arg("format", self.format.as_deref())
            .build()
    }
}

impl IntoVrlScript for CefConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_cef")
            .add_optional_arg("translate_custom_fields", self.translate_custom_fields.map(|v| v.to_string().as_str()))
            .build()
    }
}

impl IntoVrlScript for ClfConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_clf")
            .add_optional_arg("timestamp_format", self.timestamp_format.as_deref())
            .build()
    }
}

impl IntoVrlScript for CsvConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_csv")
            .add_optional_arg("delimiter", self.delimiter.map(|v| v.to_string().as_str()))
            .build()
    }
}

impl IntoVrlScript for GrokConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_grok")
            .add_arg("pattern", &self.pattern)
            .build()
    }
}

impl IntoVrlScript for KeyValueConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_key_value")
            .add_optional_arg("key_value_delimiter", self.key_value_delimiter.as_deref())
            .add_optional_arg("field_delimeter", self.field_delimeter.as_deref())
            .add_optional_arg("whitespace", self.whitespace.as_deref())
            .add_optional_arg("accept_standalone_key", self.accept_standalone_key.map(|v| v.to_string().as_str()))
            .build()
    }
}

impl IntoVrlScript for NginxLogConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_nginx_log")
            .add_arg("format", &self.format.to_string())
            .add_optional_arg("timestamp_format", self.timestamp_format.as_deref())
            .build()
    }
}

impl IntoVrlScript for XmlConfig {
    fn into_vrl_script(self) -> String {
        VrlScriptFunction::new("parse_xml")
            .add_optional_arg("include_attr", self.include_attr.map(|v| v.to_string().as_str()))
            .add_optional_arg("attr_prefix", self.attr_prefix.as_deref())
            .add_optional_arg("text_key", self.text_key.as_deref())
            .add_optional_arg("always_use_text_key", self.always_use_text_key.map(|v| v.to_string().as_str()))
            .add_optional_arg("parse_bool", self.parse_bool.map(|v| v.to_string().as_str()))
            .add_optional_arg("parse_null", self.parse_null.map(|v| v.to_string().as_str()))
            .add_optional_arg("parse_number", self.parse_number.map(|v| v.to_string().as_str()))
            .build()
    }
}

struct VrlScriptFunction(String);

impl VrlScriptFunction{
    fn new(function_name: &str) -> Self {
        Self(format!("{}!(", function_name))
    }

    fn add_arg(mut self, name: &str, value: &str) -> Self {
        self.0.push_str(&format!(", {}: {}", name, value));
        self
    }

    fn add_optional_arg(mut self, name: &str, value: Option<&str>) -> Self {
        if let Some(value) = value {
            self.add_arg(name, value);
        }
        self
    }

    fn build(mut self) -> String {
        self.0.push_str(")\n");
        self.0
    }
}
