//! Variables import and export: a file's collections as the local variables JSON (the shape the
//! REST importer reads, so exports round-trip), as CSS custom properties, as Dart constants for
//! Flutter or as a theme class for Uno, and such JSON read back into a file.

use std::collections::{BTreeMap, BTreeSet};

use figcraft_doc::{Collection, CollectionId, Document, Mode, ModeId, Variable, VariableId, VariableValue, Variables, trim_num};
use serde_json::{Value, json};

use super::CommandSpec;
use crate::codegen::{CssNames, cs_color, cs_literal, css_var_name, uno_resource_keys};
use crate::library::{is_remote_collection, local_collections};
use crate::params::{get, string};
use crate::{EngineError, Result, Session, bad, spec};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec!(
            "variable.export",
            "Export variables",
            "{collections? [ids or names; default every collection of this file, not library copies], format? (json (default): the local variables JSON that variable.import reads, colour calculations as the colour they show in each mode with other collections in their default modes | css: custom properties, the default mode on :root and each other mode on [data-<collection>=\"<mode>\"], colour calculations as the colour they show in that mode with other collections in their default modes; numbers in px unless layers or scopes use them as opacity or font weight | tailwind: a Tailwind CSS v4 stylesheet, the default mode as an @theme block named the way inspect.code's tailwind utilities read them: --color-*, --radius-*, --text-*, --spacing-*, --font-*, unique per file; each other mode as in css; colour calculations as the colour they show in that mode, other collections in their default modes) | dart: a Dart file for Flutter, a class of constants per collection and mode (Color, double, String, bool), aliases and colour calculations resolved) | uno: a C# file for Uno Platform, a static AppTheme class with a typed ThemeResourceKey per variable (colours as brushes) and CreateDictionary(), whose Light and Dark theme dictionaries come from the modes named light and dark (else the first and second mode; other collections in their default mode), aliases and colour calculations resolved), path? (write it there)}  → {text}, or {path} when written",
            false,
            export
        ),
        spec!(
            "variable.import",
            "Import variables",
            "{variables (the local variables JSON, as variable.export writes it: an object, JSON text or a path to a regular file of at most 256 MiB; `variableCollections` and `variables` under `meta` or at the top level)}  collections join this file's collection of the same name (values by mode name; new modes and variables are added, a variable of another type is skipped), others are added  → {collections (added), variables (added), updated, warnings (also when it holds no variables, or fewer than its collections list)}",
            true,
            import
        ),
    ]
}

fn export(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "variable.export";
    let doc = s.doc()?;
    let colls: Vec<&Collection> = match get(p, "collections") {
        None | Some(Value::Null) => local_collections(doc).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().and_then(|k| doc.variables.collection(k)).ok_or_else(|| bad(cmd, format!("no collection {x}"))))
            .collect::<Result<_>>()?,
        Some(_) => return Err(bad(cmd, "collections: [collection ids or names]")),
    };
    let text = match string(p, "format").as_deref().unwrap_or("json") {
        "json" => serde_json::to_string_pretty(&figcraft_format::rest::export_variables(&doc.variables, &colls))
            .map_err(|e| EngineError::Other(e.to_string()))?,
        "css" => css(doc, &colls, false),
        "tailwind" => css(doc, &colls, true),
        "dart" => dart(&doc.variables, &colls),
        "uno" => uno_theme(doc, &colls),
        _ => return Err(bad(cmd, "format: json|css|tailwind|dart|uno")),
    };
    match string(p, "path") {
        Some(path) => {
            super::file::write(&path, text.as_bytes())?;
            Ok(json!({"path": path}))
        }
        None => Ok(json!({"text": text})),
    }
}

/// `text` as a CSS comment or quoted string body.
fn css_safe(text: &str) -> String {
    text.replace("*/", "* /").replace(['"', '\\', '\n', '\r', '\u{c}'], " ")
}

/// Each collection's variables as custom properties (named as generated code names them): the
/// default mode's values on `:root`, every other mode's on `[data-<collection>="<mode>"]`;
/// aliases are `var(--name)` and colour calculations resolve to their colour in the mode.
fn css(doc: &Document, colls: &[&Collection], tailwind: bool) -> String {
    let vars = &doc.variables;
    let flattener = vars.flattener();
    // Tailwind theme names, unique per file and the same ones page code reads.
    let theme = crate::tailwind::Theme::new(doc);
    // CSS names, unique per file and the same ones generated code reads.
    let names = CssNames::new(doc);
    let name = |v: &Variable| if tailwind { theme.name(v) } else { names.name(v) };
    // Lengths need a unit where CSS reads them (`border-radius: var(--radius)`); opacity and
    // font weight don't.
    let roles = crate::token_roles::Roles::new(doc);
    let mut out = if tailwind { "@import \"tailwindcss\";\n\n".to_string() } else { String::new() };
    for c in colls {
        let attr = css_var_name(&c.name);
        for (i, m) in c.modes.iter().enumerate() {
            let sel = match i {
                // `static`: Tailwind otherwise drops theme variables no utility uses, and pages also
                // read them through `var()` and `(--name)` utilities.
                0 if tailwind => "@theme static".to_string(),
                0 => ":root".to_string(),
                _ => format!("[data-{}=\"{}\"]", attr.trim_start_matches('-'), css_safe(&m.name)),
            };
            out.push_str(&format!("/* {} · {} */\n{sel} {{\n", css_safe(&c.name), css_safe(&m.name)));
            for v in vars.variables.iter().filter(|v| v.collection == c.id) {
                // A calculation is written as the colour it resolves to in this mode (no color-mix()
                // or relative-colour output), and so is an alias to one some mode leaves out:
                // `var()` of a property no block defines would invalidate whatever reads it.
                let flat = v.values.get(&m.id).and_then(|x| flattener.value(v, &m.id, x));
                let value = match flat.as_ref() {
                    Some(VariableValue::Color(col)) => col.to_css(),
                    Some(VariableValue::Number(x)) if roles.role(v) == crate::token_roles::NumberRole::Unitless => trim_num(*x),
                    Some(VariableValue::Number(x)) => format!("{}px", trim_num(*x)),
                    Some(VariableValue::String(t)) => format!("\"{}\"", css_safe(t)),
                    Some(VariableValue::Boolean(b)) => b.to_string(),
                    Some(VariableValue::Alias(a)) => match vars.get_by_id(a) {
                        Some(t) => format!("var({})", name(t)),
                        None => continue,
                    },
                    Some(VariableValue::ColorAlpha { .. } | VariableValue::ColorMix { .. }) | None => continue,
                };
                out.push_str(&format!("  {}: {value};\n", name(v)));
            }
            out.push_str("}\n");
        }
    }
    out
}

/// Most collections the Uno theme writes.
const UNO_MAX_COLLECTIONS: usize = 64;
/// Most lines of notes at the top of the Uno theme.
const UNO_MAX_NOTES: usize = 40;

/// A name or description on one line of an XML documentation comment, at most 200 characters.
fn xml_summary(text: &str) -> String {
    let one: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect();
    one.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The variables as a C# file for Uno Platform: a `public static class AppTheme` with a
/// `ThemeResourceKey<T>` per variable (named by [`uno_resource_keys`], which layer code reads too) and
/// `CreateDictionary()`, which builds a `ResourceDictionary` with `Light` and `Dark` theme
/// dictionaries and what is the same in both in the main dictionary. A mode named light or dark (any
/// case) is that theme; only when no exported collection has one, the first mode is Light and the
/// second Dark. A collection with other mode names next to those is exported in its default mode. Aliases
/// and colour calculations are resolved per mode, other collections in their default modes. A colour is
/// always a brush; a `Color` key would come with gradients and shadows that read variables.
fn uno_theme(doc: &Document, colls: &[&Collection]) -> String {
    let vars = &doc.variables;
    let keys = uno_resource_keys(doc);
    let named = |m: &Mode, word: &str| m.name.trim().eq_ignore_ascii_case(word);
    let any_named = colls.iter().any(|c| c.modes.iter().any(|m| named(m, "light") || named(m, "dark")));
    let mut notes: Vec<String> = Vec::new();
    if colls.len() > UNO_MAX_COLLECTIONS {
        notes.push(format!("only the first {UNO_MAX_COLLECTIONS} of {} collections are exported", colls.len()));
    }
    // Declarations of the class, and the lines of the main, light and dark dictionaries.
    let (mut decls, mut main, mut light, mut dark): (Vec<String>, Vec<String>, Vec<String>, Vec<String>) = Default::default();
    for c in colls.iter().take(UNO_MAX_COLLECTIONS) {
        let (l, d): (Option<&Mode>, Option<&Mode>) = if any_named {
            match (c.modes.iter().find(|m| named(m, "light")), c.modes.iter().find(|m| named(m, "dark"))) {
                (None, None) => {
                    let m = c.modes.first();
                    if c.modes.len() > 1 {
                        let others: Vec<&str> = c.modes.iter().skip(1).map(|m| m.name.as_str()).collect();
                        notes.push(format!(
                            "{} has no light or dark mode: its default mode {} is exported; left out: {}",
                            xml_summary(&c.name),
                            m.map_or("", |m| m.name.as_str()),
                            xml_summary(&others.join(", "))
                        ));
                    }
                    (m, m)
                }
                (Some(x), None) | (None, Some(x)) => (Some(x), Some(x)),
                (Some(a), Some(b)) => (Some(a), Some(b)),
            }
        } else {
            (c.modes.first(), c.modes.get(1).or(c.modes.first()))
        };
        if let (Some(l), Some(d)) = (l, d) {
            let left: Vec<&str> = c.modes.iter().filter(|m| m.id != l.id && m.id != d.id).map(|m| m.name.as_str()).collect();
            // A collection that is not a theme was noted above with its other modes.
            if !left.is_empty() && !(any_named && l.id == d.id && !named(l, "light") && !named(l, "dark")) {
                notes.push(format!("modes not exported from {}: {}", xml_summary(&c.name), xml_summary(&left.join(", "))));
            }
        }
        let (Some(l), Some(d)) = (l, d) else { continue };
        for v in vars.variables.iter().filter(|v| v.collection == c.id) {
            let Some(key) = keys.get(&v.id) else { continue };
            let at = |m: &Mode| vars.resolve(&v.id, &BTreeMap::from([(c.id.clone(), m.id.clone())]));
            let (lv, dv) = (at(l), at(d));
            let (lv, dv) = (lv.clone().or_else(|| dv.clone()), dv.or(lv));
            let mut summary = xml_summary(&v.name);
            if !v.description.trim().is_empty() {
                summary = format!("{summary}: {}", xml_summary(&v.description));
            }
            let name = &key.name;
            let key_decl = |ty: &str| {
                format!("    /// <summary>{summary}</summary>\n    public static readonly ThemeResourceKey<{ty}> {name} = new({});", cs_literal(name))
            };
            // A value that is the same in both themes goes in the main dictionary, once.
            let mut entry = |ty: &str, a: String, b: String| {
                decls.push(key_decl(ty));
                if a == b {
                    main.push(format!("        dict[{name}.Key] = {a};"));
                } else {
                    light.push(format!("            [{name}.Key] = {a},"));
                    dark.push(format!("            [{name}.Key] = {b},"));
                }
            };
            match (key.kind, lv, dv) {
                (figcraft_doc::VariableType::Color, Some(VariableValue::Color(a)), Some(VariableValue::Color(b))) => {
                    let brush = |c| format!("new SolidColorBrush({})", cs_color(c));
                    entry("Brush", brush(a), brush(b));
                }
                // A number the same in every exported mode is a constant, which goes anywhere a number
                // does (`.Spacing(AppTheme.Space8)`); one that differs is a resource per theme.
                (figcraft_doc::VariableType::Number, Some(VariableValue::Number(a)), Some(VariableValue::Number(b))) => {
                    if !a.is_finite() || !b.is_finite() {
                        notes.push(format!("{} is not a finite number and is left out", xml_summary(&v.name)));
                    } else if a == b {
                        decls.push(format!("    /// <summary>{summary}</summary>\n    public const double {name} = {};", dart_number(a)));
                    } else {
                        entry("double", format!("{}D", dart_number(a)), format!("{}D", dart_number(b)));
                    }
                }
                (figcraft_doc::VariableType::String, Some(VariableValue::String(a)), Some(VariableValue::String(b))) => {
                    if crate::codegen::uno_font_family(v) {
                        let family = |t: &str| format!("new FontFamily({})", cs_literal(t));
                        entry("FontFamily", family(&a), family(&b));
                    } else {
                        entry("string", cs_literal(&a), cs_literal(&b));
                    }
                }
                (figcraft_doc::VariableType::Boolean, Some(VariableValue::Boolean(a)), Some(VariableValue::Boolean(b))) => {
                    if a == b {
                        decls.push(format!("    /// <summary>{summary}</summary>\n    public const bool {name} = {a};"));
                    } else {
                        notes.push(format!(
                            "{} differs by mode and is left out (a boolean is written only when it is the same in every mode)",
                            xml_summary(&v.name)
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    let mut body = String::new();
    for d in &decls {
        body.push_str(&format!("{d}\n\n"));
    }
    body.push_str("    /// <summary>Builds the dictionary. Merge it once, for example into Application.Current.Resources.</summary>\n");
    body.push_str("    public static ResourceDictionary CreateDictionary()\n    {\n        var dict = new ResourceDictionary();\n");
    for m in &main {
        body.push_str(&format!("{m}\n"));
    }
    for (var, theme, lines) in [("light", "Light", &light), ("dark", "Dark", &dark)] {
        if lines.is_empty() {
            continue;
        }
        body.push_str(&format!("        var {var} = new ResourceDictionary\n        {{\n"));
        for l in lines {
            body.push_str(&format!("{l}\n"));
        }
        body.push_str(&format!("        }};\n        dict.ThemeDictionaries[\"{theme}\"] = {var};\n"));
    }
    body.push_str("        return dict;\n    }\n");
    let mut out = String::from("// Variables exported from FigCraft.\n");
    for n in notes.iter().take(UNO_MAX_NOTES) {
        out.push_str(&format!("// note: {n}\n"));
    }
    if notes.len() > UNO_MAX_NOTES {
        out.push_str(&format!("// note: {} more notes left out\n", notes.len() - UNO_MAX_NOTES));
    }
    if body.contains("Color.FromArgb(") {
        out.push_str("using Windows.UI;\n");
    }
    out.push_str(&format!("\nnamespace MyApp;\n\n/// <summary>Theme resources exported from the design's variables.</summary>\npublic static class AppTheme\n{{\n{body}}}\n"));
    out
}

/// Words Dart never takes as a name.
const DART_RESERVED: &[&str] = &[
    "assert", "await", "break", "case", "catch", "class", "const", "continue", "default", "do", "else", "enum", "extends", "false", "final",
    "finally", "for", "if", "in", "is", "new", "null", "rethrow", "return", "super", "switch", "this", "throw", "true", "try", "var", "void",
    "while", "with", "yield",
];

/// Dart's built-in identifiers (no type may take them), and the types the constants use or name,
/// which a class or constant of that name would shadow.
const DART_BUILT_IN: &[&str] = &[
    "abstract",
    "as",
    "bool",
    "Color",
    "covariant",
    "deferred",
    "double",
    "dynamic",
    "export",
    "extension",
    "external",
    "factory",
    "Function",
    "get",
    "implements",
    "import",
    "int",
    "interface",
    "late",
    "library",
    "List",
    "mixin",
    "num",
    "Object",
    "operator",
    "part",
    "required",
    "set",
    "static",
    "String",
    "typedef",
];

/// `name` as a Dart identifier: its words (anything but ASCII letters and digits separates them)
/// in camelCase, or PascalCase for a type; never empty, never starting with a digit or reserved.
fn dart_ident(name: &str, pascal: bool) -> String {
    let mut out = String::new();
    for w in name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        let mut cs = w.chars();
        let Some(first) = cs.next() else { continue };
        if out.is_empty() && !pascal {
            out.push(first.to_ascii_lowercase());
        } else {
            out.push(first.to_ascii_uppercase());
        }
        out.extend(cs);
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, if pascal { 'V' } else { 'v' });
    }
    if DART_RESERVED.contains(&out.as_str()) || DART_BUILT_IN.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// `base`, or `base2`, `base3`… when `taken` has it already; the result is taken.
fn dart_unique(base: String, taken: &mut BTreeSet<String>) -> String {
    let mut name = base.clone();
    let mut n = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}{n}");
        n += 1;
    }
    name
}

/// `text` inside a Dart single-quoted string.
fn dart_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// `x` at full precision as a Dart literal: whole numbers as integers (`4`), the rest in Rust's
/// shortest round-trip form (`0.125`, `1e21`), which Dart reads as the same double.
fn dart_number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 { format!("{x}") } else { format!("{x:?}") }
}

/// `text` on one line of a Dart comment.
fn dart_comment(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Each collection's variables as Dart constants for Flutter: a class per mode (the default mode's
/// named after the collection, the others after the collection and the mode) with a `static const`
/// per variable. Aliases are resolved to their value in that mode (a variable of another
/// collection in that collection's default mode), so each class stands alone.
fn dart(vars: &Variables, colls: &[&Collection]) -> String {
    let mut out = String::from("// Variables exported from FigCraft.\n\nimport 'dart:ui';\n");
    let mut classes = BTreeSet::new();
    for c in colls {
        let base = dart_ident(&c.name, true);
        let mut members = BTreeSet::new();
        let names: Vec<(&Variable, String)> =
            vars.variables.iter().filter(|v| v.collection == c.id).map(|v| (v, dart_unique(dart_ident(&v.name, false), &mut members))).collect();
        for (i, m) in c.modes.iter().enumerate() {
            let class = dart_unique(if i == 0 { base.clone() } else { format!("{base}{}", dart_ident(&m.name, true)) }, &mut classes);
            out.push_str(&format!("\n/// {} · {}\nabstract final class {class} {{\n", dart_comment(&c.name), dart_comment(&m.name)));
            let modes = BTreeMap::from([(c.id.clone(), m.id.clone())]);
            for (v, name) in &names {
                let (ty, value) = match vars.resolve(&v.id, &modes) {
                    Some(VariableValue::Color(col)) => ("Color", crate::codegen::kotlin_color(col)),
                    Some(VariableValue::Number(x)) if x.is_finite() => ("double", dart_number(x)),
                    Some(VariableValue::String(t)) => ("String", format!("'{}'", dart_string(&t))),
                    Some(VariableValue::Boolean(b)) => ("bool", b.to_string()),
                    _ => continue,
                };
                out.push_str(&format!("  static const {ty} {name} = {value};\n"));
            }
            out.push_str("}\n");
        }
    }
    out
}

fn import(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "variable.import";
    let v =
        super::file::variables_param(cmd, p)?.ok_or_else(|| bad(cmd, "`variables` (the variables JSON: an object, JSON text or a path) required"))?;
    let (src, mut warnings) = figcraft_format::rest::read_variables(&v).ok_or_else(|| bad(cmd, "not a variables JSON"))?;
    let doc = s.doc_mut()?;
    // Imported collection → (this file's collection, imported mode → its mode).
    let mut colls: BTreeMap<CollectionId, (CollectionId, BTreeMap<ModeId, ModeId>)> = BTreeMap::new();
    let mut added_colls = 0usize;
    for c in &src.collections {
        // Never a library copy (read-only) or an extended collection (its modes are inherited).
        let same = doc.variables.collections.iter().find(|x| x.name == c.name && x.extends.is_none() && !is_remote_collection(doc, &x.id));
        let cid = match same.map(|x| x.id.clone()) {
            Some(id) => id,
            None => {
                let id = doc.new_collection_id();
                let name = super::variables::free_collection_name(doc, &c.name);
                doc.variables.collections.push(Collection { id: id.clone(), name, modes: Vec::new(), extends: None });
                added_colls += 1;
                id
            }
        };
        let mut modes = BTreeMap::new();
        for m in &c.modes {
            let coll = doc.variables.collections.iter().find(|x| x.id == cid);
            let mid = match coll.and_then(|x| x.modes.iter().find(|y| y.name == m.name)) {
                Some(y) => y.id.clone(),
                None => {
                    let first = coll.and_then(|x| x.default_mode()).cloned();
                    let Some(id) = doc.new_mode_id(&cid) else { continue };
                    if let Some(x) = doc.variables.collections.iter_mut().find(|x| x.id == cid) {
                        x.modes.push(Mode { id: id.clone(), name: m.name.clone() });
                    }
                    // Its variables start with their default mode's values (as with variable.addMode).
                    for var in doc.variables.variables.iter_mut().filter(|x| x.collection == cid) {
                        if let Some(val) = first.as_ref().and_then(|f| var.values.get(f)).cloned() {
                            var.values.insert(id.clone(), val);
                        }
                    }
                    id
                }
            };
            modes.insert(m.id.clone(), mid);
        }
        colls.insert(c.id.clone(), (cid, modes));
    }
    // Every variable's id here first (aliases point at any of them), then the values.
    let mut ids: BTreeMap<VariableId, VariableId> = BTreeMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    for v in &src.variables {
        let Some((cid, _)) = colls.get(&v.collection) else { continue };
        match doc.variables.variables.iter().find(|x| x.collection == *cid && x.name == v.name) {
            Some(x) if x.kind != v.kind => {
                warnings.push(format!("`{}` is a {} variable in this file: the {} one was skipped", v.name, x.kind.id(), v.kind.id()));
            }
            Some(x) => {
                ids.insert(v.id.clone(), x.id.clone());
                updated += 1;
            }
            None => {
                let id = doc.new_variable_id();
                let blank = Variable {
                    id: id.clone(),
                    collection: cid.clone(),
                    values: BTreeMap::new(),
                    extends: None,
                    overrides: BTreeSet::new(),
                    ..v.clone()
                };
                doc.variables.variables.push(blank);
                ids.insert(v.id.clone(), id);
                added += 1;
            }
        }
    }
    for v in &src.variables {
        let (Some(local), Some((cid, modes))) = (ids.get(&v.id), colls.get(&v.collection)) else { continue };
        for (m, val) in &v.values {
            let Some(lm) = modes.get(m) else { continue };
            let val = match val {
                VariableValue::Alias(a) => {
                    let Some(var) = doc.variables.variables.iter().find(|x| x.id == *local) else { continue };
                    let target = ids.get(a).ok_or_else(|| "its alias target wasn't imported".to_string());
                    match target.and_then(|t| super::variables::alias_target_by_id(&doc.variables, var, t)) {
                        Ok(t) => VariableValue::Alias(t),
                        Err(e) => {
                            warnings.push(format!("`{}`: {e}", v.name));
                            continue;
                        }
                    }
                }
                other => other.clone(),
            };
            if let Some(slot) = doc.variables.variables.iter_mut().find(|x| x.id == *local) {
                slot.values.insert(lm.clone(), val);
            }
        }
        // A value in every mode of its collection: modes the JSON lacks take the default mode's
        // (a new variable whose values all failed starts like one made with variable.create).
        let all: Vec<ModeId> =
            doc.variables.collections.iter().find(|x| x.id == *cid).map(|x| x.modes.iter().map(|m| m.id.clone()).collect()).unwrap_or_default();
        if let Some(slot) = doc.variables.variables.iter_mut().find(|x| x.id == *local) {
            let first = all.iter().find_map(|m| slot.values.get(m)).cloned().unwrap_or_else(|| VariableValue::default_for(slot.kind));
            for m in all {
                slot.values.entry(m).or_insert_with(|| first.clone());
            }
        }
    }
    Ok(json!({"collections": added_colls, "variables": added, "updated": updated, "warnings": warnings}))
}
