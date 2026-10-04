use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::info;

use crate::{
    bd::{Bd, Bead, Workdir},
    github::GitHub,
    server::config::ProjectSync,
    sync::linked,
};

const PROJECT_QUERY: &str = r#"query($id: ID!) {
  node(id: $id) {
    ... on ProjectV2 {
      fields(first: 100) { nodes {
        __typename
        ... on ProjectV2Field { id name dataType }
        ... on ProjectV2SingleSelectField { id name dataType options { id name } }
        ... on ProjectV2IterationField {
          id name dataType configuration {
            iterations { id title startDate duration }
            completedIterations { id title startDate duration }
          }
        }
      } }
    }
  }
}"#;

const ITEMS_QUERY: &str = r#"query($id: ID!, $endCursor: String) {
  node(id: $id) {
    ... on ProjectV2 {
      items(first: 100, after: $endCursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          content { ... on Issue { number repository { nameWithOwner } } }
          fieldValues(first: 100) { nodes {
            ... on ProjectV2ItemFieldSingleSelectValue {
              name optionId updatedAt field {
                ... on ProjectV2Field { id name }
                ... on ProjectV2SingleSelectField { id name }
                ... on ProjectV2IterationField { id name }
              }
            }
            ... on ProjectV2ItemFieldNumberValue {
              number updatedAt field {
                ... on ProjectV2Field { id name }
                ... on ProjectV2SingleSelectField { id name }
                ... on ProjectV2IterationField { id name }
              }
            }
            ... on ProjectV2ItemFieldIterationValue {
              iterationId title startDate duration updatedAt field {
                ... on ProjectV2Field { id name }
                ... on ProjectV2SingleSelectField { id name }
                ... on ProjectV2IterationField { id name }
              }
            }
          } }
        }
      }
    }
  }
}"#;

const UPDATE_FIELD: &str = r#"mutation($project: ID!, $item: ID!, $field: ID!, $value: ProjectV2FieldValue!) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $project, itemId: $item, fieldId: $field, value: $value
  }) { projectV2Item { id } }
}"#;

#[derive(Clone)]
struct Field {
    id: String,
    data_type: Option<String>,
    options: BTreeMap<String, String>,
    iterations: Vec<Iteration>,
}

#[derive(Clone)]
struct Iteration {
    id: String,
    start: String,
    duration: i64,
}

pub async fn sync(wd: &Workdir, gh: &GitHub, config: &ProjectSync) -> Result<bool> {
    let schema = gh
        .graphql(PROJECT_QUERY, json!({"id": config.project_id}))
        .await
        .context("loading GitHub Project v2 fields")?;
    let fields = schema["node"]["fields"]["nodes"]
        .as_array()
        .context("GitHub project did not return fields")?;
    let status = single_select_field(fields, &config.status_field)?;
    let priority = single_select_field(fields, &config.priority_field)?;
    let estimate = config
        .estimate_field
        .as_deref()
        .map(|name| field(fields, name))
        .transpose()?;
    if estimate
        .as_ref()
        .is_some_and(|field| field.data_type.as_deref() != Some("NUMBER"))
    {
        bail!("configured estimate field must be a numeric GitHub project field");
    }
    let iteration = config
        .iteration_field
        .as_deref()
        .map(|name| iteration_field(fields, name))
        .transpose()?;

    let items = gh
        .graphql_nodes(
            ITEMS_QUERY,
            json!({"id": config.project_id}),
            &["node", "items"],
        )
        .await
        .context("listing GitHub Project v2 items")?;
    let beads = Bd::new(wd).export().await?;
    let linked = linked(&beads, &config.repo);
    let by_id: BTreeMap<&str, &Bead> = beads.iter().map(|bead| (bead.id.as_str(), bead)).collect();
    let mut bead_updates = 0;
    let mut github_updates = 0;

    for item in items {
        let content = &item["content"];
        if !content["repository"]["nameWithOwner"]
            .as_str()
            .is_some_and(|repo| repo.eq_ignore_ascii_case(&config.repo))
        {
            continue;
        }
        let Some(number) = content["number"].as_u64() else {
            continue;
        };
        let Some(id) = linked.get(&number) else {
            continue;
        };
        let bead = by_id[id.as_str()];
        let values = item["fieldValues"]["nodes"]
            .as_array()
            .context("GitHub project item did not return field values")?;
        let mut changes = Vec::new();

        {
            let value = value_for(values, &status.id);
            sync_single_select(
                gh,
                config,
                &item,
                bead,
                &status,
                value,
                &config.status,
                "status",
                &mut changes,
                &mut github_updates,
            )
            .await?;
        }
        {
            let value = value_for(values, &priority.id);
            sync_single_select(
                gh,
                config,
                &item,
                bead,
                &priority,
                value,
                &config.priority,
                "priority",
                &mut changes,
                &mut github_updates,
            )
            .await?;
        }
        if let Some(field) = &estimate {
            if let Some(value) = value_for(values, &field.id) {
                let number = value["number"].as_f64().map(|n| n.round() as i64);
                if let Some(number) = number {
                    let key = number.to_string();
                    if should_take_project(value["updatedAt"].as_str(), bead.updated_at.as_deref())
                    {
                        if bead.estimate != Some(number) {
                            changes.extend(["--estimate".into(), key]);
                        }
                    } else if let Some(estimate) = bead.estimate
                        && value["number"].as_f64() != Some(estimate as f64)
                    {
                        update_github(gh, config, &item, field, json!({"number": estimate}))
                            .await?;
                        github_updates += 1;
                    }
                }
            } else if let Some(estimate) = bead.estimate {
                update_github(gh, config, &item, field, json!({"number": estimate})).await?;
                github_updates += 1;
            }
        }
        if let Some(field) = &iteration {
            if let Some(value) = value_for(values, &field.id) {
                if let Some(current) = value["iterationId"].as_str() {
                    if should_take_project(value["updatedAt"].as_str(), bead.updated_at.as_deref())
                    {
                        if let Some(iter) = field.iterations.iter().find(|i| i.id == current)
                            && let Some(due) = iteration_due(iter)
                            && bead.defer_until.as_deref() != Some(&due)
                        {
                            changes.extend(["--defer".into(), due]);
                        }
                    } else if let Some(due) = bead.defer_until.as_deref()
                        && let Some(iter) =
                            field.iterations.iter().find(|i| iteration_contains(i, due))
                        && current != iter.id
                    {
                        update_github(gh, config, &item, field, json!({"iterationId": iter.id}))
                            .await?;
                        github_updates += 1;
                    }
                }
            } else if let Some(due) = bead.defer_until.as_deref()
                && let Some(iter) = field.iterations.iter().find(|i| iteration_contains(i, due))
            {
                update_github(gh, config, &item, field, json!({"iterationId": iter.id})).await?;
                github_updates += 1;
            }
        }

        if !changes.is_empty() {
            Bd::new(wd).update_fields(&bead.id, &changes).await?;
            bead_updates += 1;
        }
    }
    if bead_updates > 0 || github_updates > 0 {
        info!(repo = %config.repo, bead_updates, github_updates, "synced GitHub Project v2 fields");
    }
    Ok(bead_updates > 0)
}

pub async fn publish(wd: &Workdir, config: &crate::server::config::Config) -> Result<()> {
    let transport = match config.transport {
        crate::sync::Transport::Dolt => true,
        crate::sync::Transport::Jsonl => false,
        crate::sync::Transport::Auto => {
            wd.output(
                "git",
                &["ls-remote", "--exit-code", "origin", "refs/dolt/data"],
            )
            .await?
            .success
        }
    };
    let bd = Bd::new(wd);
    if transport {
        bd.dolt_commit("bd-gh-sync: sync GitHub Project fields")
            .await?;
        bd.dolt_push().await?;
        return Ok(());
    }
    let path = ".beads/issues.jsonl";
    bd.export_to(std::path::Path::new(path)).await?;
    wd.run("git", &["add", "--", path]).await?;
    if wd
        .output("git", &["diff", "--cached", "--quiet"])
        .await?
        .success
    {
        return Ok(());
    }
    wd.run(
        "git",
        &["commit", "-m", "beads: sync GitHub Project fields"],
    )
    .await?;
    wd.run("git", &["push", "origin", "HEAD"]).await?;
    Ok(())
}

fn field(fields: &[Value], name: &str) -> Result<Field> {
    let value = fields
        .iter()
        .find(|field| field["name"].as_str() == Some(name))
        .with_context(|| format!("GitHub project field {name:?} was not found"))?;
    Ok(Field {
        id: value["id"]
            .as_str()
            .context("project field has no id")?
            .to_string(),
        data_type: value["dataType"].as_str().map(str::to_string),
        options: value["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| {
                Some((
                    option["name"].as_str()?.to_string(),
                    option["id"].as_str()?.to_string(),
                ))
            })
            .collect(),
        iterations: value["configuration"]["iterations"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                value["configuration"]["completedIterations"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            )
            .filter_map(|iteration| {
                Some(Iteration {
                    id: iteration["id"].as_str()?.to_string(),
                    start: iteration["startDate"].as_str()?.to_string(),
                    duration: iteration["duration"].as_i64()?,
                })
            })
            .collect(),
    })
}

fn single_select_field(fields: &[Value], name: &str) -> Result<Field> {
    let value = field(fields, name)?;
    if value.data_type.as_deref() != Some("SINGLE_SELECT") || value.options.is_empty() {
        bail!("GitHub project field {name:?} is not a single-select field");
    }
    Ok(value)
}

fn iteration_field(fields: &[Value], name: &str) -> Result<Field> {
    let value = field(fields, name)?;
    if value.data_type.as_deref() != Some("ITERATION") || value.iterations.is_empty() {
        bail!("GitHub project field {name:?} is not an iteration field or has no iterations");
    }
    Ok(value)
}

fn value_for<'a>(values: &'a [Value], field_id: &str) -> Option<&'a Value> {
    values.iter().find(|value| value["field"]["id"] == field_id)
}

#[allow(clippy::too_many_arguments)]
async fn sync_single_select(
    gh: &GitHub,
    config: &ProjectSync,
    item: &Value,
    bead: &Bead,
    field: &Field,
    value: Option<&Value>,
    mapping: &BTreeMap<String, String>,
    bead_field: &str,
    changes: &mut Vec<String>,
    updates: &mut usize,
) -> Result<()> {
    let key = match bead_field {
        "status" => bead.status.as_str().map(str::to_string),
        _ => bead.priority.as_i64().map(|p| p.to_string()),
    };
    match value {
        Some(value)
            if should_take_project(value["updatedAt"].as_str(), bead.updated_at.as_deref()) =>
        {
            let Some(option) = value["name"].as_str() else {
                return Ok(());
            };
            let mapped = mapping
                .iter()
                .find(|(_, name)| name.as_str() == option)
                .map(|(key, _)| key);
            if let Some(mapped) = mapped
                && key.as_deref() != Some(mapped)
            {
                match bead_field {
                    "status" => {
                        changes.extend(["--status".into(), mapped.clone()]);
                        if mapped == "closed" {
                            changes.push("--force".into());
                        }
                    }
                    _ => changes.extend(["--priority".into(), mapped.clone()]),
                }
            }
        }
        value => {
            if let Some(key) = key
                && let Some(name) = mapping.get(&key)
                && let Some(option_id) = field.options.get(name)
                && value.is_none_or(|v| v["optionId"].as_str() != Some(option_id))
            {
                update_github(
                    gh,
                    config,
                    item,
                    field,
                    json!({"singleSelectOptionId": option_id}),
                )
                .await?;
                *updates += 1;
            }
        }
    }
    Ok(())
}

async fn update_github(
    gh: &GitHub,
    config: &ProjectSync,
    item: &Value,
    field: &Field,
    value: Value,
) -> Result<()> {
    gh.graphql(
        UPDATE_FIELD,
        json!({
            "project": config.project_id,
            "item": item["id"],
            "field": field.id,
            "value": value
        }),
    )
    .await
    .context("updating GitHub Project v2 field")?;
    Ok(())
}

fn should_take_project(project_updated: Option<&str>, bead_updated: Option<&str>) -> bool {
    match (project_updated, bead_updated) {
        (Some(project), Some(bead)) => timestamp(project) >= timestamp(bead),
        (Some(_), None) => true,
        _ => true,
    }
}

fn timestamp(value: &str) -> Option<i64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-').filter_map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()?, date.next()?, date.next()?);
    let time = time.trim_end_matches('Z');
    let (clock, offset) = if let Some(pos) = time.rfind(['+', '-']) {
        (&time[..pos], Some(&time[pos..]))
    } else {
        (time, None)
    };
    let mut parts = clock.split(':');
    let (hour, minute) = (
        parts.next()?.parse::<i64>().ok()?,
        parts.next()?.parse::<i64>().ok()?,
    );
    let seconds_ms = (parts.next()?.parse::<f64>().ok()? * 1000.0).round() as i64;
    let mut y = year;
    let m = month;
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let offset_seconds = offset.map_or(0, |offset| {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut pieces = offset[1..]
            .split(':')
            .filter_map(|part| part.parse::<i64>().ok());
        sign * (pieces.next().unwrap_or(0) * 3600 + pieces.next().unwrap_or(0) * 60)
    });
    Some((days * 86400 + hour * 3600 + minute * 60 - offset_seconds) * 1000 + seconds_ms)
}

fn iteration_due(iteration: &Iteration) -> Option<String> {
    Some(add_days(&iteration.start, iteration.duration - 1))
}

fn iteration_contains(iteration: &Iteration, date: &str) -> bool {
    let Some(due) = iteration_due(iteration) else {
        return false;
    };
    let date = date.get(..10).unwrap_or(date);
    date >= iteration.start.as_str() && date <= due.as_str()
}

fn add_days(date: &str, days: i64) -> String {
    let mut parts = date.split('-').filter_map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (
        parts.next().unwrap_or(1970),
        parts.next().unwrap_or(1),
        parts.next().unwrap_or(1),
    );
    let total = days_from_civil(year, month, day) + days;
    let (y, m, d) = civil_from_days(total);
    format!("{y:04}-{m:02}-{d:02}")
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let era = days.div_euclid(146097);
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
