//! One test per shape the generator knows: each feeds a small Rust source
//! through [`generate_from_sources`] and asserts the exact TypeScript.

use super::{HEADER, generate_from_sources};

const VERSION: &str = r#"{"version": 1, "supported": [1]}"#;

/// The output after the header and the version constants.
fn body(lib_rs: &str) -> String {
    let out = generate_from_sources(lib_rs, "", "", VERSION).expect("the source generates");
    let prefix = format!(
        "{HEADER}\n\nexport const PROTOCOL_VERSION = 1;\n\nexport const SUPPORTED_PROTOCOL_VERSIONS: readonly number[] = [1];\n\n"
    );
    out.strip_prefix(&prefix)
        .expect("the output opens with the header and the version constants")
        .to_owned()
}

/// The error the source fails with, with its context chain.
fn error(lib_rs: &str) -> String {
    generate_from_sources(lib_rs, "", "", VERSION)
        .err()
        .map(|e| format!("{e:#}"))
        .expect("the source is refused")
}

#[test]
fn internally_tagged_enum_writes_unit_struct_and_newtype_variants_inline() {
    let src = r#"
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Inner {
    pub a: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    ListRepos,
    AddRepo { path: String, name: Option<String> },
    SessionUpdated(Inner),
}
"#;
    assert_eq!(
        body(src),
        "export interface Inner {\n  a: number;\n}\n\nexport type Msg =\n  | { type: \"list_repos\" }\n  | { type: \"add_repo\"; path: string; name: string | null }\n  | ({ type: \"session_updated\" } & Inner);\n"
    );
}

#[test]
fn adjacently_tagged_enum_puts_the_newtype_under_its_content_key() {
    let src = r#"
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ContainerRef {
    Repo(String),
    Workspace(String),
    Nothing,
}
"#;
    assert_eq!(
        body(src),
        "export type ContainerRef =\n  | { kind: \"repo\"; id: string }\n  | { kind: \"workspace\"; id: string }\n  | { kind: \"nothing\" };\n"
    );
}

#[test]
fn untagged_enum_is_the_union_of_its_variants_inner_types() {
    let src = r"
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionValue {
    Bool(bool),
    Text(String),
    Null,
}
";
    assert_eq!(
        body(src),
        "export type OptionValue =\n  | boolean\n  | string\n  | null;\n"
    );
}

#[test]
fn transparent_newtype_is_an_alias_of_its_field() {
    let src = r"
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);
";
    assert_eq!(body(src), "export type AgentId = string;\n");
}

#[test]
fn plain_unit_enum_is_a_string_literal_union() {
    let src = r#"
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionOrigin {
    Local,
    RemotePeer,
    Unknown,
}
"#;
    assert_eq!(
        body(src),
        "export type ConnectionOrigin =\n  | \"local\"\n  | \"remote_peer\"\n  | \"unknown\";\n"
    );
}

#[test]
fn serde_other_variant_is_left_out_of_the_union() {
    let src = r#"
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Fast,
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Delay { ms: u32 },
    #[serde(other)]
    Unknown,
}
"#;
    assert_eq!(
        body(src),
        "export type Mode =\n  | \"fast\";\n\nexport type Step =\n  | { kind: \"delay\"; ms: number };\n"
    );
}

#[test]
fn default_and_skip_serializing_if_make_a_field_optional() {
    let src = r#"
#[derive(Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    #[serde(default)]
    pub supported_versions: Vec<u32>,
    #[serde(default = "default_true")]
    pub use_worktree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub a: bool,
}
"#;
    assert_eq!(
        body(src),
        "export interface Welcome {\n  protocol_version: number;\n  supported_versions?: number[];\n  use_worktree?: boolean;\n  daemon_id?: string | null;\n}\n\nexport interface Prefs {\n  a?: boolean;\n}\n"
    );
}

#[test]
fn option_is_a_null_union() {
    let src = r"
#[derive(Serialize, Deserialize)]
pub struct Tip {
    pub tip: Option<String>,
    pub picks: Vec<Option<u64>>,
    pub nested: Option<Vec<String>>,
}
";
    assert_eq!(
        body(src),
        "export interface Tip {\n  tip: string | null;\n  picks: (number | null)[];\n  nested: string[] | null;\n}\n"
    );
}

#[test]
fn field_and_variant_renames_are_honoured() {
    let src = r#"
#[derive(Serialize, Deserialize)]
pub struct Tool {
    #[serde(default, rename = "tool_kind")]
    pub kind: String,
    pub r#type: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    #[serde(rename = "hi")]
    Hello,
    #[serde(rename_all = "camelCase")]
    SetName { new_name: String },
}
"#;
    assert_eq!(
        body(src),
        "export interface Tool {\n  tool_kind?: string;\n  type: string;\n}\n\nexport type Msg =\n  | { type: \"hi\" }\n  | { type: \"set_name\"; newName: string };\n"
    );
}

#[test]
fn chrono_date_time_is_a_string() {
    let src = r"
#[derive(Serialize, Deserialize)]
pub struct Stamp {
    pub at: DateTime<Utc>,
    pub when: chrono::DateTime<chrono::Utc>,
    pub ended: Option<DateTime<Utc>>,
}
";
    assert_eq!(
        body(src),
        "export interface Stamp {\n  at: string;\n  when: string;\n  ended: string | null;\n}\n"
    );
}

#[test]
fn maps_tuples_and_arrays_of_unions() {
    let src = r"
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionValue {
    Bool(bool),
    Text(String),
    Null,
}

pub type OptionValues = std::collections::BTreeMap<String, OptionValue>;

#[derive(Serialize, Deserialize)]
pub struct Bag {
    pub options: OptionValues,
    pub counts: HashMap<String, u32>,
    pub env: Vec<(String, String)>,
    pub flags: Vec<Option<bool>>,
    pub tree: Box<Vec<f64>>,
}
";
    assert_eq!(
        body(src),
        "export type OptionValue =\n  | boolean\n  | string\n  | null;\n\nexport type OptionValues = Record<string, OptionValue>;\n\nexport interface Bag {\n  options: OptionValues;\n  counts: Record<string, number>;\n  env: [string, string][];\n  flags: (boolean | null)[];\n  tree: number[];\n}\n"
    );
}

#[test]
fn doc_comments_become_jsdoc() {
    let src = r#"
/// A thing.
///
/// Second paragraph.
#[derive(Serialize, Deserialize)]
pub struct Thing {
    /// The id.
    pub id: String,
}

/// A message.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    /// Say hello.
    /// Twice.
    Hello {
        /// Who says it.
        from: String,
        to: String,
    },
    /// Stop.
    Stop,
}
"#;
    assert_eq!(
        body(src),
        "/**\n * A thing.\n *\n * Second paragraph.\n */\nexport interface Thing {\n  /** The id. */\n  id: string;\n}\n\n/** A message. */\nexport type Msg =\n  /**\n   * Say hello.\n   * Twice.\n   *\n   * `from`: Who says it.\n   */\n  | { type: \"hello\"; from: string; to: string }\n  /** Stop. */\n  | { type: \"stop\" };\n"
    );
}

#[test]
fn constants_come_from_the_version_file_lib_rs_and_error_codes() {
    let lib_rs = r#"
/// The feature.
pub const FEATURE_X: &str = "x";
const PRIVATE: &str = "p";
pub const NUM: u32 = 3;
"#;
    let error_codes = r#"
/// Bad JSON.
pub const MALFORMED_MESSAGE: &str = "malformed_message";
pub const INTERNAL: &str = "internal";
pub const ALL: &[&str] = &[MALFORMED_MESSAGE, INTERNAL];
"#;
    let out = generate_from_sources(
        lib_rs,
        "",
        error_codes,
        r#"{"version": 25, "supported": [24, 25]}"#,
    )
    .expect("the source generates");
    assert_eq!(
        out,
        format!(
            "{HEADER}\n\nexport const PROTOCOL_VERSION = 25;\n\nexport const SUPPORTED_PROTOCOL_VERSIONS: readonly number[] = [24, 25];\n\n/** The feature. */\nexport const FEATURE_X = \"x\";\n\nexport const ErrorCode = {{\n  /** Bad JSON. */\n  MALFORMED_MESSAGE: \"malformed_message\",\n  INTERNAL: \"internal\",\n}} as const;\n"
        )
    );
}

#[test]
fn an_unknown_serde_attribute_is_an_error_naming_it() {
    let field = error(
        r"
#[derive(Serialize, Deserialize)]
pub struct Outer {
    #[serde(flatten)]
    pub inner: Inner,
}

#[derive(Serialize, Deserialize)]
pub struct Inner {
    pub a: u32,
}
",
    );
    assert!(
        field.contains("Outer.inner") && field.contains("flatten"),
        "{field}"
    );
    let container = error(
        r"
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strict {
    pub a: u32,
}
",
    );
    assert!(
        container.contains("Strict") && container.contains("deny_unknown_fields"),
        "{container}"
    );
}

#[test]
fn an_unknown_field_type_is_an_error_naming_it() {
    let err = error(
        r"
#[derive(Serialize, Deserialize)]
pub struct Thing {
    pub id: Uuid,
}
",
    );
    assert!(err.contains("Thing.id") && err.contains("Uuid"), "{err}");
}

#[test]
fn unsupported_shapes_are_errors() {
    let tuple = error(
        r"
#[derive(Serialize, Deserialize)]
pub struct Pair(pub u32, pub u32);
",
    );
    assert!(tuple.contains("Pair"), "{tuple}");
    let external = error(
        r"
#[derive(Serialize, Deserialize)]
pub enum Shape {
    Circle { r: f64 },
}
",
    );
    assert!(external.contains("Shape::Circle"), "{external}");
}

#[test]
fn cfg_test_modules_and_types_without_serde_are_skipped() {
    let src = r"
#[derive(Debug)]
pub struct Plain {
    pub a: u32,
}

#[derive(Serialize)]
pub struct Shown {
    pub a: u32,
}

#[cfg(test)]
mod tests {
    #[derive(Serialize)]
    pub struct Hidden {
        pub a: u32,
    }
}
";
    assert_eq!(body(src), "export interface Shown {\n  a: number;\n}\n");
}
