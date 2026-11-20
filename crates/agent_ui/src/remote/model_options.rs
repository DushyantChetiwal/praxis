use acp_thread::AgentSessionConfigOptions;
use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use gpui::{App, Task};
use settings::AgentConfigOptionValue;

use crate::config_options::ConfigOptionsView;

fn first_select_option_for_category(
    config_options: &dyn AgentSessionConfigOptions,
    category: acp::SessionConfigOptionCategory,
) -> Option<(acp::SessionConfigId, acp::SessionConfigSelect)> {
    for option in config_options.config_options() {
        if option.category.as_ref() == Some(&category)
            && let acp::SessionConfigKind::Select(select) = option.kind
        {
            let has_choices = match &select.options {
                acp::SessionConfigSelectOptions::Ungrouped(options) => !options.is_empty(),
                acp::SessionConfigSelectOptions::Grouped(groups) => {
                    groups.iter().any(|group| !group.options.is_empty())
                }
                _ => false,
            };
            if has_choices {
                return Some((option.config_id, select));
            }
        }
    }
    None
}

fn flatten_select_options(
    options: &acp::SessionConfigSelectOptions,
) -> Vec<(Option<String>, acp::SessionConfigSelectOption)> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| (None, option.clone()))
            .collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group
                    .options
                    .iter()
                    .map(|option| (Some(group.name.clone()), option.clone()))
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn has_selectable_model_option(view: &ConfigOptionsView) -> bool {
    let (config_options, _, _) = view.remote_handles();
    !config_options.is_read_only()
        && first_select_option_for_category(
            config_options.as_ref(),
            acp::SessionConfigOptionCategory::Model,
        )
        .is_some()
}

pub(super) fn selected_model_option(view: &ConfigOptionsView) -> Option<(String, String)> {
    let (config_options, _, _) = view.remote_handles();
    let (_, select) = first_select_option_for_category(
        config_options.as_ref(),
        acp::SessionConfigOptionCategory::Model,
    )?;
    let current = select.current_value;
    let name = flatten_select_options(&select.options)
        .into_iter()
        .find(|(_, option)| option.value == current)
        .map(|(_, option)| option.name)
        .unwrap_or_else(|| current.0.to_string());
    Some((current.0.to_string(), name))
}

pub(super) fn model_options(
    view: &ConfigOptionsView,
) -> Option<(String, Vec<(Option<String>, acp_thread::AgentModelInfo)>)> {
    let (config_options, _, _) = view.remote_handles();
    let (_, select) = first_select_option_for_category(
        config_options.as_ref(),
        acp::SessionConfigOptionCategory::Model,
    )?;
    let current = select.current_value.0.to_string();
    let items = flatten_select_options(&select.options)
        .into_iter()
        .map(|(group, option)| {
            (
                group,
                acp_thread::AgentModelInfo {
                    id: acp_thread::AgentModelId::from(option.value.0.to_string()),
                    name: option.name.into(),
                    description: option.description.map(Into::into),
                    icon: None,
                    is_latest: false,
                    cost: None,
                    disabled: None,
                },
            )
        })
        .collect();
    Some((current, items))
}

pub(super) fn select_model_option(
    view: &ConfigOptionsView,
    value_id: &str,
    cx: &mut App,
) -> Task<Result<()>> {
    let (config_options, agent_server, fs) = view.remote_handles();
    if config_options.is_read_only() {
        return Task::ready(Err(anyhow::anyhow!("Session configuration is read-only")));
    }
    let Some((config_id, select)) = first_select_option_for_category(
        config_options.as_ref(),
        acp::SessionConfigOptionCategory::Model,
    ) else {
        return Task::ready(Err(anyhow::anyhow!(
            "This agent does not support model selection"
        )));
    };
    if !flatten_select_options(&select.options)
        .iter()
        .any(|(_, option)| option.value.0.as_ref() == value_id)
    {
        return Task::ready(Err(anyhow::anyhow!(
            "That model is no longer available. Refresh the model list."
        )));
    }

    let config_options = config_options.clone();
    let agent_server = agent_server.clone();
    let fs = fs.clone();
    let response = config_options.set_config_option(
        config_id.clone(),
        acp::SessionConfigOptionValue::id(acp::SessionConfigValueId::new(value_id.to_string())),
        cx,
    );
    cx.spawn(async move |cx| {
        let updated = response.await?;
        if let Some(confirmed) = updated
            .iter()
            .find(|option| option.config_id == config_id)
            .and_then(|option| match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    flatten_select_options(&select.options)
                        .iter()
                        .any(|(_, item)| item.value == select.current_value)
                        .then(|| AgentConfigOptionValue::ValueId(select.current_value.0.to_string()))
                }
                _ => None,
            })
        {
            cx.update(|cx| {
                agent_server.set_default_config_option(
                    config_id.0.as_ref(),
                    Some(confirmed),
                    fs,
                    cx,
                );
            });
        }
        Ok(())
    })
}
