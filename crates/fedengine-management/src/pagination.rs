//! HTTP continuation binds the published snapshot, source and exact filters.
use fedengine_contract::CatalogPage;
use fedengine_core::{EngineError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogParameters {
    pub object: Option<String>,
    #[serde(alias = "snapshot_version")]
    pub snapshot_id: Option<String>,
    pub schema: Option<String>,
    pub object_type: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub include: Option<String>,
    pub page_token: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    source: String,
    schema: Option<String>,
    object: Option<String>,
    object_type: Option<String>,
    include: Option<String>,
    snapshot_id: String,
    offset: usize,
}
impl CatalogParameters {
    pub fn resolve(mut self, source: &str) -> Result<Self> {
        if let Some(include) = &mut self.include {
            let mut fields = include.split(',').map(str::trim).collect::<Vec<_>>();
            fields.sort_unstable();
            fields.dedup();
            if fields.iter().any(|s| {
                !matches!(
                    *s,
                    "statistics"
                        | "storage"
                        | "activity"
                        | "column_statistics"
                        | "core"
                        | "column_properties"
                        | "constraints"
                        | "indexes"
                        | "comments"
                        | "view_definitions"
                        | "status"
                )
            }) {
                return Err(EngineError::config("invalid catalog include category"));
            }
            *include = fields.join(",");
        }
        if let Some(token) = &self.page_token {
            if !token.is_ascii() || token.len() > 8192 || token.len() % 2 != 0 {
                return Err(EngineError::config("invalid catalog page token"));
            }
            let bytes = (0..token.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&token[i..i + 2], 16))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| EngineError::config("invalid catalog page token"))?;
            let cursor: Cursor = serde_json::from_slice(&bytes)
                .map_err(|_| EngineError::config("invalid catalog page token"))?;
            if cursor.source != source
                || cursor.schema != self.schema
                || cursor.object != self.object
                || cursor.object_type != self.object_type
                || cursor.include != self.include
                || self
                    .snapshot_id
                    .as_ref()
                    .is_some_and(|s| s != &cursor.snapshot_id)
                || self.offset.is_some_and(|n| n != cursor.offset)
            {
                return Err(EngineError::new(
                    "40001",
                    "catalog page token filters changed; restart pagination",
                ));
            }
            self.snapshot_id = Some(cursor.snapshot_id);
            self.offset = Some(cursor.offset);
        }
        Ok(self)
    }
    pub fn response(&self, source: &str, page: &CatalogPage) -> Result<Value> {
        let mut value = serde_json::to_value(page)
            .map_err(|_| EngineError::new("XX000", "catalog serialization failed"))?;
        value["snapshot_version"] = json!(page.snapshot_id);
        let offset = page.offset.saturating_add(page.objects.len());
        value["next_page_token"] = if offset < page.total {
            let bytes = serde_json::to_vec(&Cursor {
                source: source.into(),
                schema: self.schema.clone(),
                object: self.object.clone(),
                object_type: self.object_type.clone(),
                include: self.include.clone(),
                snapshot_id: page.snapshot_id.clone(),
                offset,
            })
            .map_err(|_| EngineError::new("XX000", "catalog token serialization failed"))?;
            Value::String(bytes.iter().map(|b| format!("{b:02x}")).collect())
        } else {
            Value::Null
        };
        if let Some(include) = &self.include {
            let has = |s: &str| include.split(',').any(|v| v == s);
            for object in value["objects"].as_array_mut().into_iter().flatten() {
                if !has("indexes") {
                    object.as_object_mut().unwrap().remove("indexes");
                }
                let metadata = object["metadata"].as_object_mut().unwrap();
                for (field, category) in [
                    ("statistics", "statistics"),
                    ("storage", "storage"),
                    ("activity", "activity"),
                    ("column_statistics", "column_statistics"),
                    ("columns", "column_properties"),
                    ("constraints", "constraints"),
                    ("indexes", "indexes"),
                    ("comment", "comments"),
                    ("view_definition", "view_definitions"),
                    ("view_dependencies", "view_definitions"),
                    ("status", "status"),
                ] {
                    if !has(category) {
                        metadata.remove(field);
                    }
                }
            }
        }
        Ok(value)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_bind_filters_snapshot_and_reject_malformed_input() {
        let page = CatalogPage {
            snapshot_id: "snap".into(),
            incarnation: "life".into(),
            revision: 1,
            generation: 1,
            refreshed_at: "now".into(),
            stale: false,
            schemas: vec![],
            total: 2,
            offset: 0,
            objects: vec![],
        };
        let q = CatalogParameters {
            schema: Some("sales".into()),
            include: Some("status".into()),
            ..Default::default()
        }
        .resolve("crm")
        .unwrap();
        let token = q.response("crm", &page).unwrap()["next_page_token"]
            .as_str()
            .unwrap()
            .to_string();
        let next = CatalogParameters {
            page_token: Some(token),
            ..q.clone()
        };
        assert_eq!(
            next.clone().resolve("crm").unwrap().snapshot_id.as_deref(),
            Some("snap")
        );
        assert!(next.clone().resolve("other").is_err());
        assert!(
            CatalogParameters {
                schema: Some("other".into()),
                ..next
            }
            .resolve("crm")
            .is_err()
        );
        for token in ["f", "gg", "éé"] {
            assert!(
                CatalogParameters {
                    page_token: Some(token.into()),
                    ..Default::default()
                }
                .resolve("crm")
                .is_err()
            );
        }
    }
}
