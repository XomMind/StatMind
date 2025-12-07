use std::{
    env,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

use anyhow::anyhow;
use unicode_ident::{is_xid_continue, is_xid_start};

fn get_crate_path() -> PathBuf {
    env::var("CARGO_MANIFEST_DIR")
        .expect("No CARGO_MANIFEST_DIR")
        .into()
}

fn to_identifier(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 1);

    if let Some(first) = name.chars().next() {
        if !is_xid_start(first) {
            result.push('_');
        }

        for ch in name.chars() {
            result.push(if is_xid_continue(ch) { ch } else { '_' });
        }
    }

    result
}

fn to_literal(name: &str) -> String {
    format!("r#\"{}\"#", name)
}

fn parse_file(path: PathBuf, skip_header: bool) -> anyhow::Result<Vec<(i32, String)>> {
    let mut vec: Vec<(i32, String)> = Vec::new();
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();

    if skip_header {
        lines.next();
    }

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.splitn(2, char::is_whitespace);
        let id_str = parts.next().ok_or(anyhow!("Bad format (id): {line}"))?;
        let rest = parts.next().ok_or(anyhow!("Bad format (name): {line}"))?.trim();

        let id = id_str
            .parse::<i32>()
            .map_err(|_| anyhow!("Failed to parse id: {id_str}"))?;

        // Split by double space to separate Tag from Name (if present), or just use rest
        let name = if let Some((tag, _)) = rest.split_once("  ") {
            tag.trim()
        } else {
            rest
        };

        vec.push((id, name.into()));
    }
    vec.sort_by_key(|k| k.0);
    Ok(vec)
}

fn write_enum(target: &mut File, enum_name: &str, items: &[(i32, String)]) -> anyhow::Result<()> {
    let sp = "    ";

    writeln!(target, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]")?;
    writeln!(target, "#[allow(non_camel_case_types)]")?;
    writeln!(target, "#[repr(i32)]")?;
    writeln!(target, "pub enum {} {{", enum_name)?;

    for (_, name) in items {
        writeln!(target, "{}{},", sp, to_identifier(name))?;
    }

    writeln!(target, "}}")?;
    writeln!(target)?;

    writeln!(target, "impl {} {{", enum_name)?;

    writeln!(target, "{}pub fn from_id(id: i32) -> Option<Self> {{", sp)?;
    writeln!(target, "{}{}match id {{", sp, sp)?;
    for (id, name) in items {
        writeln!(
            target,
            "{}{}{}{} => Some(Self::{}),",
            sp, sp, sp, id, to_identifier(name)
        )?;
    }
    writeln!(target, "{}{}{}_ => None,", sp, sp, sp)?;
    writeln!(target, "{}{}}}", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target)?;

    writeln!(target, "{}pub fn id(&self) -> i32 {{", sp)?;
    writeln!(target, "{}{}match self {{", sp, sp)?;
    for (id, name) in items {
        writeln!(
            target,
            "{}{}{}Self::{} => {},",
            sp, sp, sp, to_identifier(name), id
        )?;
    }
    writeln!(target, "{}{}}}", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target)?;

    writeln!(target, "{}pub fn name(&self) -> &'static str {{", sp)?;
    writeln!(target, "{}{}match self {{", sp, sp)?;
    for (_id, name) in items {
        writeln!(
            target,
            "{}{}{}Self::{} => {},",
            sp, sp, sp, to_identifier(name), to_literal(name)
        )?;
    }
    writeln!(target, "{}{}}}", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target, "}}")?;
    writeln!(target)?;

    writeln!(target, "impl TryFrom<i32> for {} {{", enum_name)?;
    writeln!(target, "{}type Error = &'static str;", sp)?;
    writeln!(
        target,
        "{}fn try_from(id: i32) -> Result<Self, Self::Error> {{", sp
    )?;
    writeln!(target, "{}{}Self::from_id(id).ok_or(\"unknown id\")", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target, "}}")?;
    writeln!(target)?;

    writeln!(target, "impl Into<i32> for {} {{", enum_name)?;
    writeln!(target, "{}fn into(self) -> i32 {{", sp)?;
    writeln!(target, "{}{}self.id()", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target, "}}")?;
    writeln!(target)?;

    writeln!(target, "impl std::fmt::Display for {} {{", enum_name)?;
    writeln!(
        target,
        "{}fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {{", sp
    )?;
    writeln!(target, "{}{}f.write_str(self.name())", sp, sp)?;
    writeln!(target, "{}}}", sp)?;
    writeln!(target, "}}")?;
    writeln!(target)?;

    Ok(())
}

fn generate() -> anyhow::Result<()> {
    let source_path = get_crate_path().join("src");
    let generated_path = source_path.join("generated.rs");

    let cell_id_path = source_path.join("cellID.txt");
    let entity_id_path = source_path.join("entityID.txt");
    let item_id_path = source_path.join("itemID.txt");
    let prop_id_path = source_path.join("propID.txt");

    println!("cargo:rerun-if-changed={}", cell_id_path.display());
    println!("cargo:rerun-if-changed={}", entity_id_path.display());
    println!("cargo:rerun-if-changed={}", item_id_path.display());
    println!("cargo:rerun-if-changed={}", prop_id_path.display());

    let mut target = File::create(&generated_path)?;

    let item_items = parse_file(item_id_path, false)?;
    write_enum(&mut target, "ItemId", &item_items)?;

    let cell_items = parse_file(cell_id_path, false)?;
    write_enum(&mut target, "CellId", &cell_items)?;

    let entity_items = parse_file(entity_id_path, true)?; // Skip header
    write_enum(&mut target, "EntityId", &entity_items)?;

    let prop_items = parse_file(prop_id_path, true)?; // Skip header
    write_enum(&mut target, "PropId", &prop_items)?;

    Ok(())
}

fn main() {
    generate().expect("Failed to generate the stuff");
}
