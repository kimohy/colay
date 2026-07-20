use anyhow::{Result, anyhow, bail};
use orchestrator_state::{ModelProfileConfig, RootConfig};
use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table, TableLike};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Preset,
    Customized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProfileReportRow {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub effort: Option<String>,
    pub description: String,
    pub source: ProfileSource,
}

pub fn effective_profile_rows(
    effective: &RootConfig,
    defaults: &RootConfig,
) -> Result<Vec<ProfileReportRow>> {
    let mut rows = Vec::with_capacity(9);
    for provider in ["codex", "claude", "gemini"] {
        for profile in ["economy", "standard", "premium"] {
            let value = profile_value(effective, provider, profile)?;
            let default = profile_value(defaults, provider, profile)?;
            rows.push(ProfileReportRow {
                provider: provider.to_owned(),
                profile: profile.to_owned(),
                model: value.model.clone(),
                effort: value.effort.clone(),
                description: profile_description(profile)?.to_owned(),
                source: if value == default {
                    ProfileSource::Preset
                } else {
                    ProfileSource::Customized
                },
            });
        }
    }
    Ok(rows)
}

fn profile_value<'a>(
    config: &'a RootConfig,
    provider: &str,
    profile: &str,
) -> Result<&'a ModelProfileConfig> {
    config
        .orchestrator
        .model_profiles
        .get(provider)
        .and_then(|profiles| profiles.get(profile))
        .ok_or_else(|| anyhow!("{provider} {profile} model profile is not configured"))
}

fn profile_description(profile: &str) -> Result<&'static str> {
    match profile {
        "economy" => Ok("fast and cost-efficient simple work"),
        "standard" => Ok("everyday development work"),
        "premium" => Ok("complex work requiring the highest quality"),
        _ => bail!("unknown model profile `{profile}`"),
    }
}

fn validate_target(provider: &str, profile: &str) -> Result<()> {
    if !["codex", "claude", "gemini"].contains(&provider) {
        bail!("unknown approved provider `{provider}`");
    }
    if !["economy", "standard", "premium"].contains(&profile) {
        bail!("unknown model profile `{profile}`");
    }
    Ok(())
}

fn validate_model(model: &str) -> Result<&str> {
    let model = model.trim();
    if model.is_empty() {
        bail!("model must not be blank");
    }
    Ok(model)
}

fn validate_effort(effort: Option<&str>) -> Result<Option<&str>> {
    match effort {
        None => Ok(None),
        Some(value @ ("low" | "medium" | "high")) => Ok(Some(value)),
        Some(value) => bail!("unsupported reasoning effort `{value}`"),
    }
}

fn ensure_table<'a>(parent: &'a mut dyn TableLike, key: &str) -> Result<&'a mut dyn TableLike> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("configuration override `{key}` must be a table"))
}

pub fn set_profile_override(
    document: &mut DocumentMut,
    provider: &str,
    profile: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<()> {
    validate_target(provider, profile)?;
    let model = validate_model(model)?;
    let effort = validate_effort(effort)?;
    let orchestrator = ensure_table(document.as_table_mut(), "orchestrator")?;
    let profiles = ensure_table(orchestrator, "model_profiles")?;
    let provider_profiles = ensure_table(profiles, provider)?;
    let profile_override = ensure_table(provider_profiles, profile)?;
    profile_override.insert("model", toml_edit::value(model));
    if let Some(effort) = effort {
        profile_override.insert("effort", toml_edit::value(effort));
    }
    Ok(())
}

pub fn reset_profile_override(
    document: &mut DocumentMut,
    provider: &str,
    profile: &str,
) -> Result<bool> {
    validate_target(provider, profile)?;
    let Some(orchestrator) = document
        .get_mut("orchestrator")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(profiles) = orchestrator
        .get_mut("model_profiles")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(provider_profiles) = profiles.get_mut(provider).and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(profile_override) = provider_profiles
        .get_mut(profile)
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let removed =
        profile_override.remove("model").is_some() | profile_override.remove("effort").is_some();
    let remove_profile = profile_override.is_empty();
    if remove_profile {
        provider_profiles.remove(profile);
    }
    let remove_provider = provider_profiles.is_empty();
    if remove_provider {
        profiles.remove(provider);
    }
    let remove_profiles = profiles.is_empty();
    if remove_profiles {
        orchestrator.remove("model_profiles");
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use orchestrator_state::RootConfig;
    use toml_edit::DocumentMut;

    use super::*;

    #[test]
    fn effective_rows_identify_builtin_and_customized_values() -> Result<()> {
        let defaults = RootConfig::default();
        let mut effective = defaults.clone();
        effective
            .orchestrator
            .model_profiles
            .get_mut("claude")
            .and_then(|profiles| profiles.get_mut("premium"))
            .ok_or_else(|| anyhow!("missing mutable claude premium profile"))?
            .model = "company-fable".to_owned();

        let rows = effective_profile_rows(&effective, &defaults)?;
        let builtin = rows
            .iter()
            .find(|row| row.provider == "codex" && row.profile == "standard")
            .ok_or_else(|| anyhow!("missing codex standard row"))?;
        let customized = rows
            .iter()
            .find(|row| row.provider == "claude" && row.profile == "premium")
            .ok_or_else(|| anyhow!("missing claude premium row"))?;
        assert_eq!(builtin.source, ProfileSource::Preset);
        assert_eq!(customized.source, ProfileSource::Customized);
        assert_eq!(customized.model, "company-fable");
        assert_eq!(rows.len(), 9);
        Ok(())
    }

    #[test]
    fn set_then_reset_changes_only_the_selected_override() -> Result<()> {
        let mut document = "config_version = 4\n# keep this comment\n".parse::<DocumentMut>()?;
        set_profile_override(
            &mut document,
            "claude",
            "premium",
            "company-fable",
            Some("high"),
        )?;
        set_profile_override(
            &mut document,
            "gemini",
            "standard",
            "company-gemini",
            Some("medium"),
        )?;
        assert!(reset_profile_override(&mut document, "claude", "premium")?);
        let text = document.to_string();
        assert!(text.contains("# keep this comment"));
        assert!(!text.contains("company-fable"));
        assert!(text.contains("company-gemini"));
        Ok(())
    }

    #[test]
    fn profile_override_rejects_blank_model_and_invalid_effort() -> Result<()> {
        let mut document = "config_version = 4\n".parse::<DocumentMut>()?;
        assert!(set_profile_override(&mut document, "codex", "standard", "  ", None).is_err());
        assert!(
            set_profile_override(
                &mut document,
                "codex",
                "standard",
                "gpt-5.6-terra",
                Some("maximum"),
            )
            .is_err()
        );
        Ok(())
    }
}
