use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use virt::nwfilter::NWFilter;

use super::ApiError;
use crate::libvirt::with_conn;

/// libvirt network filters (nwfilter): the firewall primitive applied to a
/// domain's interfaces via `<filterref>`.
pub fn routes() -> Router {
    Router::new()
        .route("/filters", get(list).post(create))
        .route("/filters/{name}", get(get_one).delete(delete))
        .route("/filters/{name}/xml", get(get_xml).put(put_xml))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let filters = c.list_all_nw_filters(0)?;
        let mut arr = Vec::with_capacity(filters.len());
        for f in &filters {
            arr.push(json!({
                "name": f.get_name().unwrap_or_default(),
                "uuid": f.get_uuid_string().unwrap_or_default(),
            }));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let f = NWFilter::lookup_by_name(c, &name)?;
        Ok(json!({
            "name": f.get_name().unwrap_or_default(),
            "uuid": f.get_uuid_string().unwrap_or_default(),
            "xml": f.get_xml_desc(0)?,
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let f = NWFilter::lookup_by_name(c, &name)?;
        Ok(json!({"name": f.get_name().unwrap_or_default(), "xml": f.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct FilterXml {
    xml: String,
}

async fn create(Json(body): Json<FilterXml>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let f = NWFilter::define_xml(c, &body.xml)?;
        Ok(json!({"name": f.get_name().unwrap_or_default(), "defined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        // define_xml replaces an existing filter of the same name.
        let _ = NWFilter::define_xml(c, &body)?;
        Ok(json!({"name": name, "defined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let f = NWFilter::lookup_by_name(c, &name)?;
        f.undefine()?;
        Ok(json!({"name": name, "undefined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}
