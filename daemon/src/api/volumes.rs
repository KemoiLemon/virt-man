use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::stream::Stream;

use super::ApiError;
use crate::disks::{self, DiskSource};
use crate::libvirt::{LvError, with_conn};

pub fn routes() -> Router {
    Router::new()
        .route("/pools/{pool}/volumes", get(list).post(create))
        .route("/pools/{pool}/volumes/{vol}", get(get_one).delete(delete))
        .route("/pools/{pool}/volumes/{vol}/upload", post(upload))
}

async fn list(Path(pool): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let vols = p.list_all_volumes(0)?;
        let mut arr = Vec::with_capacity(vols.len());
        for vol in &vols {
            arr.push(json!({
                "name": vol.get_name().unwrap_or_default(),
                "path": vol.get_path().unwrap_or_default(),
                "capacity": vol.get_info().map(|i| i.capacity).unwrap_or(0),
                "allocation": vol.get_info().map(|i| i.allocation).unwrap_or(0),
            }));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_one(Path((pool, vol)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let v = StorageVol::lookup_by_name(&p, &vol)?;
        Ok(json!({
            "name": v.get_name().unwrap_or_default(),
            "path": v.get_path().unwrap_or_default(),
            "capacity": v.get_info().map(|i| i.capacity).unwrap_or(0),
            "allocation": v.get_info().map(|i| i.allocation).unwrap_or(0),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct VolCreate {
    xml: String,
}

async fn create(
    Path(pool): Path<String>,
    Json(body): Json<VolCreate>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let vol = StorageVol::create_xml(&p, &body.xml, 0)?;
        Ok(json!({
            "name": vol.get_name().unwrap_or_default(),
            "path": vol.get_path().unwrap_or_default(),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path((pool, vol)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    // Refuse while any guest still has this disk attached: deleting it would
    // corrupt that guest's next boot, and the guest would look fine until it
    // was restarted. Checked in its own connection so the refusal can be a
    // 409 with the offending guest names rather than a blanket 500.
    let target = DiskSource::Volume {
        pool: pool.clone(),
        volume: vol.clone(),
    };
    let holders = {
        let target = target.clone();
        with_conn(move |c| Ok(disks::referencing_domains(c, &target)))
            .await
            .map_err(ApiError::internal)?
    };
    if !holders.is_empty() {
        return Err(ApiError::conflict(format!(
            "'{vol}' is in use by {} — detach it first",
            holders.join(", ")
        )));
    }

    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let v = StorageVol::lookup_by_name(&p, &vol)?;
        v.delete(0)?;
        Ok(json!({"name": vol, "deleted": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Stream a raw body into a volume. `?name=` creates a new volume in the
/// pool first; otherwise the body overwrites the named existing volume.
///
/// ponytail: the body is spooled to a temp file, then fed into libvirt's
/// blocking virStream from a second thread — avoids holding a multi-GB ISO in
/// RAM. Streaming straight into virStream needs a channel + two libvirt
/// threads; add if the extra disk write becomes a problem.
async fn upload(
    Path((pool, vol)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let create_name = q.get("name").cloned();

    let tmp = std::env::temp_dir().join(format!("virtman-{pool}-{vol}.upload"));
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ApiError::bad_request)?;
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
        total += chunk.len() as u64;
    }
    file.flush().await.map_err(ApiError::internal)?;
    drop(file);

    let tmp_path = tmp.clone();
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let vol = match &create_name {
            Some(name) => StorageVol::create_xml(&p, &volume_xml(name, total), 0)?,
            None => StorageVol::lookup_by_name(&p, &vol)?,
        };
        upload_from_file(c, &vol, &tmp_path, total)?;
        Ok(json!({
            "name": vol.get_name().unwrap_or_default(),
            "uploaded": total,
        }))
    })
    .await
    .map_err(ApiError::internal);

    let _ = std::fs::remove_file(&tmp);
    Ok(Json(v?))
}

fn upload_from_file(
    conn: &virt::connect::Connect,
    vol: &StorageVol,
    path: &std::path::Path,
    length: u64,
) -> Result<(), LvError> {
    let stream = Stream::new(conn, 0)?;
    let feeder_stream = stream.clone();
    let feeder_path = path.to_path_buf();

    // virStorageVolUpload blocks until the stream finishes, so feed it from
    // a second thread.
    let feeder = std::thread::spawn(move || -> Result<(), LvError> {
        let mut f = std::fs::File::open(&feeder_path)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            use std::io::Read;
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            feeder_stream.send(&buf[..n])?;
        }
        feeder_stream.finish()?;
        Ok(())
    });

    let upload_result = vol.upload(&stream, 0, length, 0);
    let feed_result = feeder.join();
    upload_result?;
    match feed_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(LvError::Msg("upload feeder thread panicked".into())),
    }
}

fn volume_xml(name: &str, capacity: u64) -> String {
    format!(
        "<volume><name>{name}</name><capacity>{capacity}</capacity><target><format type='raw'/></target></volume>"
    )
}
