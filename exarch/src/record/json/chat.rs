//! Complete serialization mirrors; domain checks precede semantic hydration.
use super::{
    Binary, ChatMessage, ChatRole, ContentPart, Count, CountRule, CustomPart, DecodeCause,
    DecodeFailure, Fields, JsonEncodeFailure, JsonRef, JsonShape, MessageOptions, Mirror,
    Serialize, ToolCall, ToolResponse, UnknownField, decode_structural, duplicate, external,
    length, missing, on_json_stack, required_payload,
};
#[derive(Debug, Serialize)]
pub(crate) struct RawChatMessage {
    role: ChatRole,
    content: RawMessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<MessageOptions>,
}
impl Mirror for RawChatMessage {
    type Value = ChatMessage;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { role: ChatRole => required, content: RawMessageContent => required, options: Option<MessageOptions> => optional })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.role.check().map_err(|e| e.field("role"))?;
        self.content.check().map_err(|e| e.field("content"))?;
        self.options.check().map_err(|e| e.field("options"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        ChatMessage {
            role: self.role.hydrate(),
            content: self.content.hydrate(),
            options: self.options.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            role: <ChatRole as Mirror>::from_value(&value.role).map_err(|e| e.field("role"))?,
            content: <RawMessageContent as Mirror>::from_value(&value.content)
                .map_err(|e| e.field("content"))?,
            options: <Option<MessageOptions> as Mirror>::from_value(&value.options)
                .map_err(|e| e.field("options"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawToolCall {
    call_id: String,
    fn_name: String,
    fn_arguments: RawForeignValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signatures: Option<Vec<String>>,
}
impl Mirror for RawToolCall {
    type Value = ToolCall;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { call_id: String => required, fn_name: String => required, fn_arguments: RawForeignValue => required, thought_signatures: Option<Vec<String>> => optional })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.call_id.check().map_err(|e| e.field("call_id"))?;
        self.fn_name.check().map_err(|e| e.field("fn_name"))?;
        self.fn_arguments
            .check()
            .map_err(|e| e.field("fn_arguments"))?;
        self.thought_signatures
            .check()
            .map_err(|e| e.field("thought_signatures"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        ToolCall {
            call_id: self.call_id.hydrate(),
            fn_name: self.fn_name.hydrate(),
            fn_arguments: self.fn_arguments.hydrate(),
            thought_signatures: self.thought_signatures.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            call_id: <String as Mirror>::from_value(&value.call_id)
                .map_err(|e| e.field("call_id"))?,
            fn_name: <String as Mirror>::from_value(&value.fn_name)
                .map_err(|e| e.field("fn_name"))?,
            fn_arguments: <RawForeignValue as Mirror>::from_value(&value.fn_arguments)
                .map_err(|e| e.field("fn_arguments"))?,
            thought_signatures: <Option<Vec<String>> as Mirror>::from_value(
                &value.thought_signatures,
            )
            .map_err(|e| e.field("thought_signatures"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawCustomPart {
    model_iden: Option<genai::ModelIden>,
    data: RawForeignValue,
}
impl Mirror for RawCustomPart {
    type Value = CustomPart;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { model_iden: Option<genai::ModelIden> => optional, data: RawForeignValue => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.model_iden.check().map_err(|e| e.field("model_iden"))?;
        self.data.check().map_err(|e| e.field("data"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        CustomPart {
            model_iden: self.model_iden.hydrate(),
            data: self.data.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            model_iden: <Option<genai::ModelIden> as Mirror>::from_value(&value.model_iden)
                .map_err(|e| e.field("model_iden"))?,
            data: <RawForeignValue as Mirror>::from_value(&value.data)
                .map_err(|e| e.field("data"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) enum RawContentPart {
    Text(String),
    Binary(Binary),
    ToolCall(RawToolCall),
    ToolResponse(ToolResponse),
    ThoughtSignature(String),
    ReasoningContent(String),
    Custom(RawCustomPart),
}
impl Mirror for RawContentPart {
    type Value = ContentPart;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "Text",
                    "Binary",
                    "ToolCall",
                    "ToolResponse",
                    "ThoughtSignature",
                    "ReasoningContent",
                    "Custom",
                ],
            )?;
            match tag {
                "Text" => <String>::read(required_payload(payload, tag)?)
                    .map(Self::Text)
                    .map_err(|e| e.payload(tag)),
                "Binary" => <Binary>::read(required_payload(payload, tag)?)
                    .map(Self::Binary)
                    .map_err(|e| e.payload(tag)),
                "ToolCall" => <RawToolCall>::read(required_payload(payload, tag)?)
                    .map(Self::ToolCall)
                    .map_err(|e| e.payload(tag)),
                "ToolResponse" => <ToolResponse>::read(required_payload(payload, tag)?)
                    .map(Self::ToolResponse)
                    .map_err(|e| e.payload(tag)),
                "ThoughtSignature" => <String>::read(required_payload(payload, tag)?)
                    .map(Self::ThoughtSignature)
                    .map_err(|e| e.payload(tag)),
                "ReasoningContent" => <String>::read(required_payload(payload, tag)?)
                    .map(Self::ReasoningContent)
                    .map_err(|e| e.payload(tag)),
                "Custom" => <RawCustomPart>::read(required_payload(payload, tag)?)
                    .map(Self::Custom)
                    .map_err(|e| e.payload(tag)),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Text(value) => value.check().map_err(|e| e.payload("Text")),
            Self::Binary(value) => value.check().map_err(|e| e.payload("Binary")),
            Self::ToolCall(value) => value.check().map_err(|e| e.payload("ToolCall")),
            Self::ToolResponse(value) => value.check().map_err(|e| e.payload("ToolResponse")),
            Self::ThoughtSignature(value) => {
                value.check().map_err(|e| e.payload("ThoughtSignature"))
            }
            Self::ReasoningContent(value) => {
                value.check().map_err(|e| e.payload("ReasoningContent"))
            }
            Self::Custom(value) => value.check().map_err(|e| e.payload("Custom")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Text(value) => ContentPart::Text(value.hydrate()),
            Self::Binary(value) => ContentPart::Binary(value.hydrate()),
            Self::ToolCall(value) => ContentPart::ToolCall(value.hydrate()),
            Self::ToolResponse(value) => ContentPart::ToolResponse(value.hydrate()),
            Self::ThoughtSignature(value) => ContentPart::ThoughtSignature(value.hydrate()),
            Self::ReasoningContent(value) => ContentPart::ReasoningContent(value.hydrate()),
            Self::Custom(value) => ContentPart::Custom(value.hydrate()),
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            ContentPart::Text(value) => <String as Mirror>::from_value(value)
                .map(Self::Text)
                .map_err(|e| e.payload("Text")),
            ContentPart::Binary(value) => <Binary as Mirror>::from_value(value)
                .map(Self::Binary)
                .map_err(|e| e.payload("Binary")),
            ContentPart::ToolCall(value) => <RawToolCall as Mirror>::from_value(value)
                .map(Self::ToolCall)
                .map_err(|e| e.payload("ToolCall")),
            ContentPart::ToolResponse(value) => <ToolResponse as Mirror>::from_value(value)
                .map(Self::ToolResponse)
                .map_err(|e| e.payload("ToolResponse")),
            ContentPart::ThoughtSignature(value) => <String as Mirror>::from_value(value)
                .map(Self::ThoughtSignature)
                .map_err(|e| e.payload("ThoughtSignature")),
            ContentPart::ReasoningContent(value) => <String as Mirror>::from_value(value)
                .map(Self::ReasoningContent)
                .map_err(|e| e.payload("ReasoningContent")),
            ContentPart::Custom(value) => <RawCustomPart as Mirror>::from_value(value)
                .map(Self::Custom)
                .map_err(|e| e.payload("Custom")),
        })
    }
}

// genai 0.7.3 message/options/binary/tool schemas are ordinary products and
// external sums. Only arbitrary Value fields need owner-local drop discipline.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawMessageContent(Vec<RawContentPart>);
impl Mirror for RawMessageContent {
    type Value = genai::chat::MessageContent;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Vec::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.0.check()
    }
    fn hydrate(&self) -> Self::Value {
        genai::chat::MessageContent::from_parts(self.0.hydrate())
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        v.iter()
            .enumerate()
            .map(|(i, p)| RawContentPart::from_value(p).map_err(|e| e.index(i.into())))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(Debug)]
pub(crate) enum RawForeignValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}
impl Serialize for RawForeignValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => s.serialize_unit(),
            Self::Bool(v) => v.serialize(s),
            Self::Number(v) => v.serialize(s),
            Self::String(v) => v.serialize(s),
            Self::Array(v) => v.serialize(s),
            Self::Object(v) => {
                use serde::ser::SerializeMap as _;
                let mut map = s.serialize_map(Some(v.len()))?;
                for (key, value) in v {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}
impl Mirror for RawForeignValue {
    type Value = serde_json::Value;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            if let Some(items) = v.items() {
                items
                    .enumerate()
                    .map(|(i, v)| Self::read(v).map_err(|e| e.index(i.into())))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Self::Array)
            } else if let Some(mut members) = v.members() {
                let mut values: Vec<(String, Self)> = Vec::new();
                if let Some((key, value)) = members.next() {
                    // Pinned Value treats this key specially only in the first slot.
                    if key == "$serde_json::private::RawValue" {
                        let text: String = decode_structural(value).map_err(|e| e.field(key))?;
                        let document = ral_core::data::json::JsonDocument::parse(text.as_bytes())
                            .map_err(|e| {
                            DecodeFailure::new(DecodeCause::JsonSyntax(e)).field(key)
                        })?;
                        let raw = Self::read(document.root()).map_err(|e| e.field(key))?;
                        if members.len() != 0 {
                            return Err(length(CountRule::Exactly(1u64.into()), 1 + members.len()));
                        }
                        return Ok(raw);
                    }
                    values.push((key.to_owned(), Self::read(value).map_err(|e| e.field(key))?));
                }
                for (key, value) in members {
                    let value = Self::read(value).map_err(|e| e.field(key))?;
                    if let Some((_, slot)) = values.iter_mut().find(|(name, _)| name == key) {
                        *slot = value;
                    } else {
                        values.push((key.to_owned(), value));
                    }
                }
                Ok(Self::Object(values))
            } else {
                match v.shape() {
                    JsonShape::Null => Ok(Self::Null),
                    JsonShape::Bool => decode_structural(v).map(Self::Bool),
                    JsonShape::Number => decode_structural(v).map(Self::Number),
                    JsonShape::String => decode_structural(v).map(Self::String),
                    JsonShape::Array | JsonShape::Object => {
                        unreachable!("containers handled above")
                    }
                }
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(v) => serde_json::Value::Bool(*v),
            Self::Number(v) => serde_json::Value::Number(v.clone()),
            Self::String(v) => serde_json::Value::String(v.clone()),
            Self::Array(v) => serde_json::Value::Array(v.iter().map(Self::hydrate).collect()),
            Self::Object(v) => {
                serde_json::Value::Object(v.iter().map(|(k, v)| (k.clone(), v.hydrate())).collect())
            }
        })
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| {
            Ok(match v {
                serde_json::Value::Array(v) => {
                    Self::Array(v.iter().map(Self::from_value).collect::<Result<_, _>>()?)
                }
                serde_json::Value::Object(v) => Self::Object(
                    v.iter()
                        .map(|(k, v)| Ok((k.clone(), Self::from_value(v)?)))
                        .collect::<Result<_, JsonEncodeFailure>>()?,
                ),
                serde_json::Value::Null => Self::Null,
                serde_json::Value::Bool(v) => Self::Bool(*v),
                serde_json::Value::Number(v) => Self::Number(v.clone()),
                serde_json::Value::String(v) => Self::String(v.clone()),
            })
        })
    }
}
impl Drop for RawForeignValue {
    fn drop(&mut self) {
        fn children(value: &mut RawForeignValue, pending: &mut Vec<RawForeignValue>) {
            match value {
                RawForeignValue::Array(items) => pending.append(items),
                RawForeignValue::Object(members) => {
                    pending.extend(std::mem::take(members).into_iter().map(|(_, value)| value));
                }
                RawForeignValue::Null
                | RawForeignValue::Bool(_)
                | RawForeignValue::Number(_)
                | RawForeignValue::String(_) => {}
            }
        }
        let mut pending = Vec::new();
        children(self, &mut pending);
        while let Some(mut value) = pending.pop() {
            children(&mut value, &mut pending);
        }
    }
}
