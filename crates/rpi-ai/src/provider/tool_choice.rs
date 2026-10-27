//! Tool-selection validation and conversion at the provider protocol boundary.

use crate::types::{
    AllowedToolsChoice, AllowedToolsMode, AllowedToolsType, Api, NativeToolChoice, ToolChoice,
    ToolChoiceMode, ToolName,
};

impl NativeToolChoice {
    fn validate_shape(&self) -> Result<(), String> {
        if self.kind().trim().is_empty() {
            return Err("native toolChoice requires a nonempty string `type`".into());
        }
        if matches!(self.kind(), "function" | "custom" | "tool") {
            let name = self
                .0
                .get("name")
                .or_else(|| self.0.get(self.kind()).and_then(|v| v.get("name")));
            if !name
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| !name.trim().is_empty())
            {
                return Err(format!(
                    "{} toolChoice requires a nonempty name",
                    self.kind()
                ));
            }
        }
        if self.kind() == "allowed_tools" {
            let value = serde_json::Value::Object(self.0.clone());
            let group = value.get("allowed_tools").unwrap_or(&value);
            if !matches!(
                group.get("mode").and_then(serde_json::Value::as_str),
                Some("auto" | "required")
            ) {
                return Err("allowed_tools mode must be auto or required".into());
            }
            let tools = group
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .ok_or("allowed_tools requires tools")?;
            if tools.is_empty() {
                return Err("allowed_tools requires at least one tool".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for tool in tools {
                let native: NativeToolChoice = serde_json::from_value(tool.clone())
                    .map_err(|e| format!("invalid allowed tool: {e}"))?;
                if native.kind() == "allowed_tools" {
                    return Err("allowed_tools cannot contain nested groups".into());
                }
                native.validate_shape()?;
                if !seen.insert(tool.to_string()) {
                    return Err("duplicate allowed tool".into());
                }
            }
        }
        Ok(())
    }

    /// Only selectors with equivalent local-tool semantics can cross protocols.
    fn portable(&self) -> Option<ToolChoice> {
        let value = serde_json::Value::Object(self.0.clone());
        let function = |value: &serde_json::Value| -> Option<ToolName> {
            if value.get("type")?.as_str()? != "function"
                || value
                    .as_object()?
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "name" | "function"))
            {
                return None;
            }
            if value.get("name").is_some() && value.get("function").is_some() {
                return None;
            }
            if value.get("function").is_some_and(|function| {
                function
                    .as_object()
                    .map_or(true, |fields| fields.keys().any(|key| key != "name"))
            }) {
                return None;
            }
            let name = value
                .get("name")
                .or_else(|| value.get("function")?.get("name"))?
                .as_str()?;
            Some(ToolName { name: name.into() })
        };
        match self.kind() {
            "function" => function(&value).map(|tool| ToolChoice::Tool { name: tool.name }),
            "allowed_tools" => {
                let nested = value.get("allowed_tools");
                if nested.is_some() && (value.get("mode").is_some() || value.get("tools").is_some())
                {
                    return None;
                }
                let group = nested.unwrap_or(&value);
                if value
                    .as_object()?
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "mode" | "tools" | "allowed_tools"))
                    || group
                        .as_object()?
                        .keys()
                        .any(|key| !matches!(key.as_str(), "type" | "mode" | "tools"))
                {
                    return None;
                }
                let tools: Option<Vec<_>> = group
                    .get("tools")?
                    .as_array()?
                    .iter()
                    .map(function)
                    .collect();
                Some(ToolChoice::AllowedTools(AllowedToolsChoice {
                    kind: AllowedToolsType::AllowedTools,
                    mode: serde_json::from_value(group.get("mode")?.clone()).ok()?,
                    tools: tools?,
                }))
            }
            _ => None,
        }
    }

    fn anthropic_value(&self) -> Result<serde_json::Value, String> {
        self.validate_shape()?;
        if let Some(portable) = self.portable() {
            return portable.anthropic_value();
        }
        match self.kind() {
            "auto" | "none" | "any" | "tool" => {
                if self.0.keys().any(|key| {
                    !matches!(key.as_str(), "type" | "name" | "disable_parallel_tool_use")
                }) {
                    return Err("unsupported Anthropic toolChoice fields".into());
                }
                if self
                    .0
                    .get("disable_parallel_tool_use")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err("disable_parallel_tool_use must be boolean".into());
                }
                Ok(serde_json::Value::Object(self.0.clone()))
            }
            _ => Err(format!(
                "toolChoice type `{}` has no Anthropic equivalent",
                self.kind()
            )),
        }
    }
}

impl ToolChoice {
    /// Validate the selected protocol, without guessing a model's capabilities.
    pub fn validate_for_api(&self, api: &Api) -> Result<(), String> {
        match api {
            Api::OpenaiCompletions
            | Api::OpenaiResponses
            | Api::AzureOpenaiResponses
            | Api::OpenaiCodexResponses => Ok(()),
            Api::AnthropicMessages => match self {
                Self::Native(native) => native.anthropic_value().map(|_| ()),
                _ => Ok(()),
            },
            // Faux accepts portable choices for deterministic host tests.
            Api::Faux if !matches!(self, Self::Native(_)) => Ok(()),
            _ => Err(format!(
                "toolChoice is not implemented for API {}",
                api.as_str()
            )),
        }
    }

    /// Return a lossless portable equivalent, when one exists.
    #[cfg(feature = "providers")]
    pub(crate) fn portable(&self) -> Option<Self> {
        match self {
            Self::Native(native) => native.portable(),
            _ => Some(self.clone()),
        }
    }

    /// Validate the selector for this invocation before starting a request.
    pub fn validate(&self, active_tools: &[String]) -> Result<(), String> {
        let names = match self {
            Self::Native(native) => {
                native.validate_shape()?;
                if let Some(portable) = native.portable() {
                    return portable.validate(active_tools);
                }
                if native.kind() == "tool" {
                    if let Some(name) = native.0.get("name").and_then(serde_json::Value::as_str) {
                        return Self::Tool { name: name.into() }.validate(active_tools);
                    }
                }
                return Ok(());
            }
            Self::Mode(_) => return Ok(()),
            Self::Tool { name } => vec![name],
            Self::AllowedTools(group) => {
                if group.tools.is_empty() {
                    return Err("allowed_tools requires at least one tool".into());
                }
                group.tools.iter().map(|tool| &tool.name).collect()
            }
        };
        let mut seen = std::collections::BTreeSet::new();
        for name in names {
            if name.trim().is_empty() {
                return Err("tool name must not be empty".into());
            }
            if !seen.insert(name) {
                return Err(format!("duplicate tool `{name}`"));
            }
            if !active_tools.contains(name) {
                return Err(format!("tool `{name}` is not active"));
            }
        }
        Ok(())
    }

    /// Encode portable selectors for OpenAI; preserve native selectors verbatim.
    #[cfg(feature = "providers")]
    pub(crate) fn openai_value(
        &self,
        responses: bool,
        is_custom: impl Fn(&str) -> bool,
    ) -> serde_json::Value {
        let tool_ref = |name: &str| {
            if responses {
                serde_json::json!({"type": if is_custom(name) { "custom" } else { "function" }, "name": name})
            } else {
                serde_json::json!({"type":"function", "function":{"name":name}})
            }
        };
        match self {
            Self::Native(native) => serde_json::Value::Object(native.0.clone()),
            Self::Mode(mode) => serde_json::to_value(mode).unwrap(),
            Self::Tool { name } => tool_ref(name),
            Self::AllowedTools(group) => {
                let tools: Vec<_> = group
                    .tools
                    .iter()
                    .map(|tool| tool_ref(&tool.name))
                    .collect();
                if responses {
                    serde_json::json!({"type":"allowed_tools", "mode":group.mode, "tools":tools})
                } else {
                    serde_json::json!({"type":"allowed_tools", "allowed_tools":{"mode":group.mode, "tools":tools}})
                }
            }
        }
    }

    /// Anthropic expresses subsets through tool definitions plus auto/any.
    pub(crate) fn anthropic_value(&self) -> Result<serde_json::Value, String> {
        Ok(match self {
            Self::Native(native) => return native.anthropic_value(),
            Self::Mode(ToolChoiceMode::Auto) => serde_json::json!({"type":"auto"}),
            Self::Mode(ToolChoiceMode::None) => serde_json::json!({"type":"none"}),
            Self::Mode(ToolChoiceMode::Required) => serde_json::json!({"type":"any"}),
            Self::Tool { name } => serde_json::json!({"type":"tool", "name":name}),
            Self::AllowedTools(group) => {
                serde_json::json!({"type":match group.mode { AllowedToolsMode::Auto => "auto", AllowedToolsMode::Required => "any" }})
            }
        })
    }
}

#[cfg(test)]
mod tool_choice_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn tool_choice_rejects_ambiguous_cross_protocol_conversion() {
        for value in [
            json!({"type":"function","name":"read","function":{"name":"write"}}),
            json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"read"}],"allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"write"}}]}}),
        ] {
            let choice: ToolChoice = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&choice).unwrap(), value);
            assert!(choice.validate_for_api(&Api::OpenaiResponses).is_ok());
            assert!(choice.validate_for_api(&Api::AnthropicMessages).is_err());
        }
    }

    #[test]
    fn tool_choice_native_protocol_checks() {
        let choice: ToolChoice =
            serde_json::from_value(json!({"type":"mcp","server_label":"docs"})).unwrap();
        for api in [
            Api::OpenaiCompletions,
            Api::OpenaiResponses,
            Api::AzureOpenaiResponses,
            Api::OpenaiCodexResponses,
        ] {
            assert!(choice.validate_for_api(&api).is_ok());
        }
        for api in [
            Api::AnthropicMessages,
            Api::GoogleGenerativeAi,
            Api::Other("unsupported".into()),
        ] {
            assert!(choice.validate_for_api(&api).is_err());
        }
        for value in [
            json!({"type":"function","name":"missing"}),
            json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"missing"}]}),
        ] {
            let choice: ToolChoice = serde_json::from_value(value).unwrap();
            assert!(choice.validate(&["read".into()]).is_err());
        }
    }

    #[test]
    fn tool_choice_group_validation() {
        let active = vec!["read".into(), "bash".into()];
        let choice: ToolChoice = serde_json::from_value(
            json!({"type":"allowed_tools","mode":"auto","tools":[{"name":"read"},{"name":"bash"}]}),
        )
        .unwrap();
        assert!(choice.validate(&active).is_ok());
        for tools in [
            json!([]),
            json!([{"name":""}]),
            json!([{"name":"missing"}]),
            json!([{"name":"read"},{"name":"read"}]),
        ] {
            let choice: ToolChoice = serde_json::from_value(
                json!({"type":"allowed_tools","mode":"required","tools":tools}),
            )
            .unwrap();
            assert!(choice.validate(&active).is_err());
        }
        for value in [
            json!({"type":"allowed_tools","mode":"none","tools":[{"name":"read"}]}),
            json!({"type":42,"mode":"auto","tools":[{"name":"read"}]}),
            json!({"type":"allowed_tools","tools":[{"name":"read"}]}),
            json!({"type":"allowed_tools","mode":"auto","tools":[{"name":"read","typo":true}]}),
        ] {
            match serde_json::from_value::<ToolChoice>(value) {
                Err(_) => {}
                Ok(choice) => assert!(choice.validate(&active).is_err()),
            }
        }
    }
}
