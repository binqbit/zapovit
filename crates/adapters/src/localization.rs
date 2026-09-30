use application::{Error, Result};
use fluent_bundle::{FluentBundle, FluentResource};
use std::collections::BTreeSet;

pub const LOCALES: [&str; 2] = ["en", "uk"];
const EN: &str = include_str!("../../../locales/en.ftl");
const UK: &str = include_str!("../../../locales/uk.ftl");
/// Only static UI messages pass through Fluent. User content is never an argument.
pub fn tr(locale: &str, key: &str) -> String {
    let selected = if locale == "uk" {
        ("uk", UK)
    } else {
        ("en", EN)
    };
    render(selected.0, selected.1, key)
        .or_else(|| render("en", EN, key))
        .unwrap_or_else(|| "Message unavailable".into())
}
pub fn state(locale: &str, value: &impl serde::Serialize) -> String {
    let name = serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "needs_attention".into());
    tr(locale, &format!("state-{}", name.replace('_', "-")))
}
fn render(locale: &str, source: &str, key: &str) -> Option<String> {
    let resource = FluentResource::try_new(source.to_owned()).ok()?;
    let mut bundle = FluentBundle::new(vec![locale.parse().ok()?]);
    bundle.set_use_isolating(false);
    bundle.add_resource(resource).ok()?;
    let pattern = bundle.get_message(key)?.value()?;
    let mut errors = Vec::new();
    let result = bundle
        .format_pattern(pattern, None, &mut errors)
        .into_owned();
    if errors.is_empty() {
        Some(result)
    } else {
        None
    }
}
pub fn validate() -> Result<()> {
    fn keys(source: &str) -> BTreeSet<&str> {
        source
            .lines()
            .filter(|l| !l.starts_with([' ', '#']) && l.contains(" = "))
            .filter_map(|l| l.split(" = ").next())
            .collect()
    }
    if keys(EN) != keys(UK) {
        return Err(Error::Config);
    }
    for key in keys(EN) {
        if render("en", EN, key).is_none() || render("uk", UK, key).is_none() {
            return Err(Error::Config);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn locales_have_identical_valid_keys() {
        super::validate().unwrap();
    }
}
