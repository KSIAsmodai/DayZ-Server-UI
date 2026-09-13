//! `cfgspawnabletypes.xml` round-trip parser (PDR §5.3, §9.3).
//!
//! Each `<type>` can carry any number of `<attachments>` and `<cargo>`
//! groups, in any order — vanilla and modded files routinely interleave
//! them (`<cargo>…<attachments>…<cargo>…`) rather than grouping all of
//! one kind together.
//!
//! The reader is **hand-rolled over quick-xml events** rather than
//! using the serde adapter, because quick-xml's serde deserializer
//! rejects interleaved repeated elements with "duplicate field" — this
//! blew up parsing for real-world files (e.g. Chernarus's own
//! `cfgspawnabletypes.xml` and mod files like SNAFU's), skipping them
//! entirely and dropping their loadouts. Same root cause already fixed
//! in `cfg_randompresets_xml.rs`; the event walker is O(n) and handles
//! any ordering.

use std::path::Path;

use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::domain::{
    AttachmentGroup, CargoGroup, ItemSource, SpawnableItem, SpawnableType,
};
use crate::error::{AppError, AppResult};

// ---------- Serialize-side schema ----------
//
// We still use serde for WRITING — we control the output ordering
// (all attachments groups, then all cargo groups, per type) so serde's
// repeated-element quirk doesn't bite on the output. The reader below
// doesn't use this.

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename = "spawnabletypes")]
struct File {
    #[serde(rename = "type", default)]
    types: Vec<XmlType>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct XmlType {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@hoarder", default, skip_serializing_if = "Option::is_none")]
    hoarder: Option<u8>,
    #[serde(rename = "attachments", default)]
    attachments: Vec<XmlGroup>,
    #[serde(rename = "cargo", default)]
    cargo: Vec<XmlGroup>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct XmlGroup {
    /// Optional on the wire — vanilla cfgspawnabletypes.xml and many mod
    /// files omit `chance=`, which CE reads as 1.0. Keeping this required
    /// was the cause of the "missing field @chance" parse error that
    /// blanked the Loadouts page on vanilla Chernarus.
    #[serde(rename = "@chance", default, skip_serializing_if = "Option::is_none")]
    chance: Option<f64>,
    #[serde(
        rename = "@slotName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    slot_name: Option<String>,
    #[serde(rename = "item", default)]
    items: Vec<XmlItem>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct XmlItem {
    #[serde(rename = "@name", default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(rename = "@chance", default, skip_serializing_if = "Option::is_none")]
    chance: Option<f64>,
    #[serde(rename = "@preset", default, skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
}

// ---------- Public API ----------

pub fn parse_file(
    path: &Path,
    workspace: &Path,
    source: ItemSource,
) -> AppResult<Vec<SpawnableType>> {
    let bytes = std::fs::read(path)?;
    let rel = rel_slash(workspace, path);
    parse_bytes(&bytes, source, &rel)
        .map_err(|e| AppError::Internal(format!("parsing {}: {e}", path.display())))
}

pub fn parse_bytes(
    bytes: &[u8],
    source: ItemSource,
    file: &str,
) -> Result<Vec<SpawnableType>, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<SpawnableType> = Vec::new();

    // The `<type>` currently being built, if we're inside one.
    let mut current_type: Option<SpawnableType> = None;

    // The `<attachments>`/`<cargo>` group currently being built, if
    // we're inside one (groups don't nest, so one level suffices).
    let mut current_group_is_cargo: Option<bool> = None;
    let mut current_group_chance = 1.0f64;
    let mut current_group_slot: Option<String> = None;
    let mut current_items: Vec<SpawnableItem> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = element_name_lower(&e)?;
                match name.as_str() {
                    "spawnabletypes" => {} // root, ignore
                    "type" => {
                        let (t_name, hoarder) = read_type_attrs(&e)?;
                        current_type = Some(SpawnableType {
                            name: t_name,
                            hoarder,
                            attachments: Vec::new(),
                            cargo: Vec::new(),
                            source,
                            mod_id: None,
                            file: file.to_string(),
                        });
                    }
                    "attachments" | "cargo" => {
                        if current_type.is_some() {
                            let (chance, slot_name) = read_group_attrs(&e)?;
                            current_group_is_cargo = Some(name == "cargo");
                            current_group_chance = chance;
                            current_group_slot = slot_name;
                            current_items.clear();
                        }
                    }
                    "item" => {
                        if current_group_is_cargo.is_some() {
                            current_items.push(read_item(&e)?);
                        }
                    }
                    _ => {} // unknown element — skip
                }
            }
            Ok(Event::Empty(e)) => {
                let name = element_name_lower(&e)?;
                match name.as_str() {
                    "type" => {
                        // Empty type, e.g. `<type name="X"/>`.
                        let (t_name, hoarder) = read_type_attrs(&e)?;
                        out.push(SpawnableType {
                            name: t_name,
                            hoarder,
                            attachments: Vec::new(),
                            cargo: Vec::new(),
                            source,
                            mod_id: None,
                            file: file.to_string(),
                        });
                    }
                    "attachments" | "cargo" => {
                        if let Some(t) = current_type.as_mut() {
                            let (chance, slot_name) = read_group_attrs(&e)?;
                            if name == "cargo" {
                                t.cargo.push(CargoGroup {
                                    chance,
                                    items: Vec::new(),
                                });
                            } else {
                                t.attachments.push(AttachmentGroup {
                                    chance,
                                    slot_name,
                                    items: Vec::new(),
                                });
                            }
                        }
                    }
                    "item" => {
                        if current_group_is_cargo.is_some() {
                            current_items.push(read_item(&e)?);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = element_name_lower_end(&e)?;
                match name.as_str() {
                    "attachments" | "cargo" => {
                        if let (Some(is_cargo), Some(t)) =
                            (current_group_is_cargo.take(), current_type.as_mut())
                        {
                            let items = std::mem::take(&mut current_items);
                            if is_cargo {
                                t.cargo.push(CargoGroup {
                                    chance: current_group_chance,
                                    items,
                                });
                            } else {
                                t.attachments.push(AttachmentGroup {
                                    chance: current_group_chance,
                                    slot_name: current_group_slot.take(),
                                    items,
                                });
                            }
                        }
                        current_group_chance = 1.0;
                        current_group_slot = None;
                    }
                    "type" => {
                        if let Some(t) = current_type.take() {
                            out.push(t);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {} // comments, text, PI — ignore
            Err(e) => return Err(e.to_string()),
        }
        buf.clear();
    }

    Ok(out)
}

fn element_name_lower(e: &BytesStart) -> Result<String, String> {
    std::str::from_utf8(e.name().as_ref())
        .map(|s| s.to_ascii_lowercase())
        .map_err(|err| err.to_string())
}

fn element_name_lower_end(e: &BytesEnd) -> Result<String, String> {
    std::str::from_utf8(e.name().as_ref())
        .map(|s| s.to_ascii_lowercase())
        .map_err(|err| err.to_string())
}

fn read_type_attrs(e: &BytesStart) -> Result<(String, bool), String> {
    let mut name = String::new();
    let mut hoarder = false;
    for a in e.attributes() {
        let a = a.map_err(|err| err.to_string())?;
        let key = std::str::from_utf8(a.key.as_ref()).map_err(|err| err.to_string())?;
        let val = a.unescape_value().map_err(|err| err.to_string())?;
        match key {
            "name" => name = val.into_owned(),
            "hoarder" => hoarder = val.trim() == "1",
            _ => {}
        }
    }
    Ok((name, hoarder))
}

fn read_group_attrs(e: &BytesStart) -> Result<(f64, Option<String>), String> {
    let mut chance = 1.0f64;
    let mut slot_name = None;
    for a in e.attributes() {
        let a = a.map_err(|err| err.to_string())?;
        let key = std::str::from_utf8(a.key.as_ref()).map_err(|err| err.to_string())?;
        let val = a.unescape_value().map_err(|err| err.to_string())?;
        match key {
            "chance" => chance = val.parse().unwrap_or(1.0),
            "slotName" => slot_name = Some(val.into_owned()),
            _ => {}
        }
    }
    Ok((chance, slot_name))
}

fn read_item(e: &BytesStart) -> Result<SpawnableItem, String> {
    let mut name = String::new();
    let mut chance = 1.0f64;
    let mut preset = None;
    for a in e.attributes() {
        let a = a.map_err(|err| err.to_string())?;
        let key = std::str::from_utf8(a.key.as_ref()).map_err(|err| err.to_string())?;
        let val = a.unescape_value().map_err(|err| err.to_string())?;
        match key {
            "name" => name = val.into_owned(),
            "chance" => chance = val.parse().unwrap_or(1.0),
            "preset" => preset = Some(val.into_owned()),
            _ => {}
        }
    }
    Ok(SpawnableItem {
        name,
        chance,
        preset,
    })
}

fn item_to(it: &SpawnableItem) -> XmlItem {
    if let Some(preset) = &it.preset {
        XmlItem {
            name: None,
            chance: Some(it.chance).filter(|c| (c - 1.0).abs() > f64::EPSILON),
            preset: Some(preset.clone()),
        }
    } else {
        XmlItem {
            name: Some(it.name.clone()),
            chance: Some(it.chance),
            preset: None,
        }
    }
}

pub fn serialize(types: &[SpawnableType]) -> AppResult<String> {
    let doc = File {
        types: types
            .iter()
            .map(|t| XmlType {
                name: t.name.clone(),
                hoarder: if t.hoarder { Some(1) } else { None },
                attachments: t
                    .attachments
                    .iter()
                    .map(|g| XmlGroup {
                        chance: Some(g.chance),
                        slot_name: g.slot_name.clone(),
                        items: g.items.iter().map(item_to).collect(),
                    })
                    .collect(),
                cargo: t
                    .cargo
                    .iter()
                    .map(|g| XmlGroup {
                        chance: Some(g.chance),
                        slot_name: None,
                        items: g.items.iter().map(item_to).collect(),
                    })
                    .collect(),
            })
            .collect(),
    };
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let body = quick_xml::se::to_string(&doc).map_err(|e| {
        AppError::Internal(format!("serializing cfgspawnabletypes.xml: {e}"))
    })?;
    out.push_str(&super::pretty::pretty(&body));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

pub fn write_types(path: &Path, types: &[SpawnableType]) -> AppResult<()> {
    let text = serialize(types)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn rel_slash(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<spawnabletypes>
  <type name="M4A1">
    <attachments chance="0.50" slotName="optic">
      <item name="ACOGOptic" chance="1.00"/>
    </attachments>
    <attachments chance="0.80">
      <item preset="weaponMagSTANAG"/>
    </attachments>
    <cargo chance="0.30">
      <item name="Mag_STANAG_30Rnd" chance="1.00"/>
    </cargo>
  </type>
</spawnabletypes>
"#;

    #[test]
    fn group_chance_is_optional_defaults_to_one() {
        // Vanilla and mods routinely omit `chance=` on groups. Keeping
        // it required caused "missing field @chance" errors that blanked
        // the Loadouts page.
        let src = r#"<?xml version="1.0" encoding="UTF-8"?>
<spawnabletypes>
  <type name="LooseCrate">
    <cargo>
      <item name="Can_Beans" chance="1.00"/>
    </cargo>
    <attachments>
      <item name="Strap" chance="1.00"/>
    </attachments>
  </type>
</spawnabletypes>
"#;
        let types = parse_bytes(src.as_bytes(), ItemSource::Vanilla, "").unwrap();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].cargo.len(), 1);
        assert_eq!(types[0].cargo[0].chance, 1.0);
        assert_eq!(types[0].attachments.len(), 1);
        assert_eq!(types[0].attachments[0].chance, 1.0);
    }

    #[test]
    fn round_trip_preserves_groups_and_preset_refs() {
        let types = parse_bytes(
            SAMPLE.as_bytes(),
            ItemSource::Vanilla,
            "cfgspawnabletypes.xml",
        )
        .unwrap();
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert_eq!(t.name, "M4A1");
        assert_eq!(t.attachments.len(), 2);
        assert_eq!(t.attachments[0].slot_name.as_deref(), Some("optic"));
        assert_eq!(t.attachments[0].items.len(), 1);
        assert_eq!(t.attachments[0].items[0].name, "ACOGOptic");
        assert_eq!(t.attachments[1].items[0].preset.as_deref(), Some("weaponMagSTANAG"));
        assert_eq!(t.cargo.len(), 1);

        let out = serialize(types.as_slice()).unwrap();
        let again = parse_bytes(out.as_bytes(), ItemSource::Vanilla, "").unwrap();
        assert_eq!(again[0].attachments.len(), 2);
        assert_eq!(
            again[0].attachments[1].items[0].preset.as_deref(),
            Some("weaponMagSTANAG")
        );
        // Ensure output keeps attributes inline (no whitespace bleed).
        assert!(
            out.contains("<item name=\"ACOGOptic\""),
            "item name attr should stay inline, got:\n{out}"
        );
    }

    #[test]
    fn interleaved_cargo_and_attachments_parse() {
        // This is the exact shape that blew up the old serde-based
        // reader with "duplicate field cargo" on real-world files
        // (Chernarus's own cfgspawnabletypes.xml, SNAFU's mod file):
        // a single <type> with more than one <cargo> group, with an
        // <attachments> group interleaved between them.
        let src = r#"<?xml version="1.0" encoding="UTF-8"?>
<spawnabletypes>
  <type name="CrashSite_Backpack" hoarder="1">
    <cargo chance="0.30">
      <item name="ItemA" chance="1.00"/>
    </cargo>
    <attachments chance="0.50">
      <item name="ItemB" chance="1.00"/>
    </attachments>
    <cargo chance="0.10">
      <item name="ItemC" chance="1.00"/>
    </cargo>
  </type>
</spawnabletypes>
"#;
        let types = parse_bytes(src.as_bytes(), ItemSource::Vanilla, "").unwrap();
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert!(t.hoarder);
        assert_eq!(t.cargo.len(), 2);
        assert_eq!(t.cargo[0].chance, 0.30);
        assert_eq!(t.cargo[0].items[0].name, "ItemA");
        assert_eq!(t.cargo[1].chance, 0.10);
        assert_eq!(t.cargo[1].items[0].name, "ItemC");
        assert_eq!(t.attachments.len(), 1);
        assert_eq!(t.attachments[0].items[0].name, "ItemB");
    }
}
