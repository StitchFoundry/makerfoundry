//! A maker's own print-settings note for a 3D model: the material, nozzle, layer height, infill,
//! supports and orientation that worked for them. Every field is optional; a note with nothing in
//! it is not kept and not shown. Kept as JSON in `mf_designs.print_settings`. (The settings read
//! out of a 3MF file are a separate thing: `profile`.)

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::AppState;
use crate::accounts::{forbidden, same_origin};
use crate::pages::escape;

const MAX_MATERIAL: usize = 40;
const MAX_NOTE: usize = 300;
/// Nozzle and layer height are in millimetres, between these.
const NOZZLE_MM: (f64, f64) = (0.1, 2.0);
const LAYER_MM: (f64, f64) = (0.02, 1.5);

/// The note as the maker fills it in (text, as typed).
#[derive(Debug, Default, Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    pub material: String,
    #[serde(default)]
    pub nozzle: String,
    #[serde(default)]
    pub layer_height: String,
    #[serde(default)]
    pub infill: String,
    #[serde(default)]
    pub supports: String,
    #[serde(default)]
    pub orientation: String,
}

/// A checked print-settings note.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nozzle_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infill_percent: Option<u8>,
    /// `Some(true)`: supports needed; `Some(false)`: none needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<String>,
}

/// Text with its edges trimmed and no control characters, or why not.
fn text(raw: &str, max: usize, what: &str) -> Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().count() > max {
        return Err(format!("{what} can be up to {max} characters."));
    }
    if t.chars().any(char::is_control) {
        return Err(format!("{what} can't have control characters."));
    }
    Ok(Some(t.to_string()))
}

/// A millimetre size such as `0.4`, within `range`.
fn millimetres(raw: &str, range: (f64, f64), what: &str) -> Result<Option<f64>, String> {
    let t = raw.trim().trim_end_matches("mm").trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() && (range.0..=range.1).contains(&v) => Ok(Some(v)),
        _ => Err(format!(
            "{what} should be a number of millimetres from {} to {}.",
            range.0, range.1
        )),
    }
}

impl Settings {
    /// Checks what the maker typed.
    pub fn from_form(f: &SettingsForm) -> Result<Settings, String> {
        let infill = f.infill.trim().trim_end_matches('%').trim();
        let infill_percent = if infill.is_empty() {
            None
        } else {
            match infill.parse::<u8>() {
                Ok(v) if v <= 100 => Some(v),
                _ => return Err("Infill should be a whole number from 0 to 100.".into()),
            }
        };
        let supports = match f.supports.as_str() {
            "" => None,
            "yes" => Some(true),
            "no" => Some(false),
            _ => return Err("Choose whether supports are needed.".into()),
        };
        Ok(Settings {
            material: text(&f.material, MAX_MATERIAL, "Material")?,
            nozzle_mm: millimetres(&f.nozzle, NOZZLE_MM, "Nozzle")?,
            layer_mm: millimetres(&f.layer_height, LAYER_MM, "Layer height")?,
            infill_percent,
            supports,
            orientation: text(&f.orientation, MAX_NOTE, "The orientation note")?,
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == Settings::default()
    }

    /// The form's fields, filled with what is kept.
    pub fn form_html(&self, id: i64) -> String {
        let value = |v: Option<String>| escape(&v.unwrap_or_default());
        let pick = |want: Option<bool>, label: &str, v: &str| {
            let on = if self.supports == want { " selected" } else { "" };
            format!(r#"<option value="{v}"{on}>{label}</option>"#)
        };
        format!(
            r#"<h2>Print settings</h2><form method="post" action="/3d/{id}/settings" class="form">
<label>Material <small>(for example PLA or PETG)</small><input name="material" maxlength="{MAX_MATERIAL}" value="{material}"></label>
<label>Nozzle <small>(mm)</small><input name="nozzle" inputmode="decimal" value="{nozzle}"></label>
<label>Layer height <small>(mm)</small><input name="layer_height" inputmode="decimal" value="{layer}"></label>
<label>Infill <small>(percent)</small><input name="infill" inputmode="numeric" value="{infill}"></label>
<label>Supports<select name="supports">{none}{yes}{no}</select></label>
<label>Orientation <small>(how to lay it on the bed)</small><textarea name="orientation" maxlength="{MAX_NOTE}" rows="3">{orientation}</textarea></label>
<button type="submit" class="secondary">Save print settings</button>
</form>"#,
            material = value(self.material.clone()),
            nozzle = value(self.nozzle_mm.map(|v| v.to_string())),
            layer = value(self.layer_mm.map(|v| v.to_string())),
            infill = value(self.infill_percent.map(|v| v.to_string())),
            none = pick(None, "Not said", ""),
            yes = pick(Some(true), "Needed", "yes"),
            no = pick(Some(false), "Not needed", "no"),
            orientation = value(self.orientation.clone()),
        )
    }

    /// The card on a model's page; nothing at all when the note is empty.
    pub fn card_html(&self) -> String {
        let mut rows = Vec::new();
        let mut row = |label: &str, v: String| rows.push(format!("<dt>{label}</dt><dd>{}</dd>", escape(&v)));
        if let Some(v) = &self.material {
            row("Material", v.clone());
        }
        if let Some(v) = self.nozzle_mm {
            row("Nozzle", format!("{v} mm"));
        }
        if let Some(v) = self.layer_mm {
            row("Layer height", format!("{v} mm"));
        }
        if let Some(v) = self.infill_percent {
            row("Infill", format!("{v}%"));
        }
        if let Some(v) = self.supports {
            row("Supports", if v { "Needed" } else { "Not needed" }.to_string());
        }
        if let Some(v) = &self.orientation {
            row("Orientation", v.clone());
        }
        if rows.is_empty() {
            return String::new();
        }
        format!(
            r#"<section class="print-settings card"><h2>Print settings</h2><dl>{}</dl></section>"#,
            rows.concat()
        )
    }
}

/// A model's kept note (empty when there is none or it can't be read).
pub async fn load(pool: &PgPool, id: i64) -> Settings {
    let v: Option<Option<serde_json::Value>> =
        sqlx::query_scalar("SELECT print_settings FROM mf_designs WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    v.flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// `POST /3d/<id>/settings`: keeps (or, left empty, clears) the maker's print-settings note.
pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<SettingsForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (user, pool, _) = match super::upload::own_model(&state, &headers, id).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let settings = match Settings::from_form(&f) {
        Ok(s) => s,
        Err(why) => return crate::designs::manage_page(&state, &pool, &user, id, &why).await,
    };
    let value = if settings.is_empty() {
        None
    } else {
        serde_json::to_value(&settings).ok()
    };
    if let Err(e) = sqlx::query("UPDATE mf_designs SET print_settings = $2 WHERE id = $1")
        .bind(id)
        .bind(value)
        .execute(&pool)
        .await
    {
        eprintln!("model3d: print settings of {id}: {e}");
        return crate::designs::manage_page(
            &state,
            &pool,
            &user,
            id,
            "The print settings couldn't be kept just now. Please try again in a few minutes.",
        )
        .await;
    }
    Redirect::to(&format!("/designs/{id}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(material: &str, nozzle: &str, layer: &str, infill: &str, supports: &str, note: &str) -> SettingsForm {
        SettingsForm {
            material: material.into(),
            nozzle: nozzle.into(),
            layer_height: layer.into(),
            infill: infill.into(),
            supports: supports.into(),
            orientation: note.into(),
        }
    }

    #[test]
    fn an_empty_form_is_an_empty_note_and_shows_nothing() {
        let s = Settings::from_form(&SettingsForm::default()).unwrap();
        assert!(s.is_empty());
        assert_eq!(s.card_html(), "");
    }

    #[test]
    fn a_full_note_is_kept_and_shown() {
        let s = Settings::from_form(&form(" PETG ", "0.4 mm", "0.2", "15%", "yes", "Flat side down")).unwrap();
        assert_eq!(s.material.as_deref(), Some("PETG"));
        assert_eq!(s.nozzle_mm, Some(0.4));
        assert_eq!(s.infill_percent, Some(15));
        assert_eq!(s.supports, Some(true));
        let html = s.card_html();
        for want in ["PETG", "0.4 mm", "0.2 mm", "15%", "Needed", "Flat side down"] {
            assert!(html.contains(want), "{want}: {html}");
        }
    }

    #[test]
    fn limits_are_enforced() {
        for bad in [
            form(&"x".repeat(41), "", "", "", "", ""),
            form("", "0", "", "", "", ""),
            form("", "abc", "", "", "", ""),
            form("", "NaN", "", "", "", ""),
            form("", "", "9", "", "", ""),
            form("", "", "inf", "", "", ""),
            form("", "", "", "101", "", ""),
            form("", "", "", "-1", "", ""),
            form("", "", "", "", "maybe", ""),
            form("", "", "", "", "", &"y".repeat(301)),
            form("PL\u{0}A", "", "", "", "", ""),
        ] {
            assert!(Settings::from_form(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn typed_text_is_escaped_when_shown() {
        let s = Settings::from_form(&form("<b>PLA</b>", "", "", "", "", "")).unwrap();
        assert!(s.card_html().contains("&lt;b&gt;"));
        assert!(!s.card_html().contains("<b>"));
    }

    #[test]
    fn a_kept_note_round_trips_through_json() {
        let s = Settings::from_form(&form("PLA", "0.4", "0.16", "20", "no", "")).unwrap();
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}
