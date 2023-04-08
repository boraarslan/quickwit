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
