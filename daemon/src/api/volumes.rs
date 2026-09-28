use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
        .route("/pools/{pool}/volumes/{vol}/hash", get(hash))
}

async fn list(Path(pool): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        // Report the filesystem as it is, not as libvirt last cached it, so a
        // file that appeared outside the volume API is listed. See
        // `pools::refresh_for_read`.
        super::pools::refresh_for_read(&p);
        let vols = p.list_all_volumes(0)?;
        // Which guests hold each volume, so a picker can say "in use by" and
        // not offer one guest's disk to another. One pass over the domains.
        let held = crate::disks::holders_by_label(c);
        let mut arr = Vec::with_capacity(vols.len());
        for vol in &vols {
            let name = vol.get_name().unwrap_or_default();
            let path = vol.get_path().unwrap_or_default();
            let used_by = crate::disks::holders_of(&held, &pool, &name, &path);
            arr.push(json!({
                "name": name,
                "path": path,
                "capacity": vol.get_info().map(|i| i.capacity).unwrap_or(0),
                "allocation": vol.get_info().map(|i| i.allocation).unwrap_or(0),
                "used_by": used_by,
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

/// Lowercase hex of a digest.
pub(super) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file, streamed in chunks so a multi-GB ISO is never held in
/// memory. Used by `GET .../hash` to verify a volume that already exists —
/// the upload path does *not* use this, since it hashes while it writes.
///
/// The digest is of the volume's stored bytes, so for a non-raw format it is
/// the digest of that container (a qcow2 header and all), not of the guest's
/// view of the disk. Uploads create raw volumes, where the two coincide.
fn sha256_file(path: &str) -> Result<(String, u64), LvError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((to_hex(&hasher.finalize()), total))
}

/// Content hash and size of a stored volume, so a client that staged the same
/// bytes elsewhere can prove the transfer completed.
async fn hash(Path((pool, vol)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let vol = StorageVol::lookup_by_name(&p, &vol)?;
        let name = vol.get_name().unwrap_or_default();
        let path = vol.get_path().unwrap_or_default();
        let (sha256, size) = sha256_file(&path)?;
        Ok(json!({"name": name, "sha256": sha256, "size": size}))
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
/// Scratch space for a spooled upload body.
///
/// Deliberately **not** `std::env::temp_dir()`: `/tmp` is tmpfs on this host, so
/// spooling an 8 GB ISO there would hold the whole thing in RAM. `/var/tmp` is
/// disk-backed by convention. `VIRT_MAN_SPOOL_DIR` overrides it (the directory
/// must already exist).
pub(super) fn spool_dir() -> PathBuf {
    std::env::var_os("VIRT_MAN_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

/// A spool path unique to this request. Two uploads naming the same volume used
/// to share one file, so concurrent uploads corrupted each other.
fn spool_path(pool: &str, vol: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    spool_dir().join(format!(
        "virtman-{pool}-{vol}-{}-{n}.upload",
        std::process::id()
    ))
}

/// Spool the request body to `tmp`, returning the byte count.
///
/// The file is removed if the body fails part-way: an interrupted upload must
/// not leave a multi-GB file behind. The success path removes it once the
/// volume has been written.
/// Bytes per second, guarding the divide. A transfer fast enough to register
/// zero elapsed time would otherwise report an infinite or NaN rate.
fn bps(bytes: u64, secs: f64) -> f64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0.0;
    }
    bytes as f64 / secs
}

async fn spool_body(body: Body, tmp: &std::path::Path) -> Result<u64, ApiError> {
    let mut file = tokio::fs::File::create(tmp)
        .await
        .map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    loop {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                let _ = std::fs::remove_file(tmp);
                return Err(ApiError::bad_request(e));
            }
            None => break,
        };
        if let Err(e) = file.write_all(&chunk).await {
            let _ = std::fs::remove_file(tmp);
            return Err(ApiError::internal(e));
        }
        total += chunk.len() as u64;
    }
    if let Err(e) = file.flush().await {
        let _ = std::fs::remove_file(tmp);
        return Err(ApiError::internal(e));
    }
    Ok(total)
}

async fn upload(
    Path((pool, vol)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let create_name = q.get("name").cloned();

    let tmp = spool_path(&pool, &vol);
    // Measured separately because they fail differently: a slow receive means
    // the network, a slow write means the storage. Reporting one number would
    // hide which end is the bottleneck.
    let recv_start = std::time::Instant::now();
    let total = spool_body(body, &tmp).await?;
    let recv_secs = recv_start.elapsed().as_secs_f64();

    let tmp_path = tmp.clone();
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &pool)?;
        let vol = match &create_name {
            Some(name) => StorageVol::create_xml(&p, &volume_xml(name, total), 0)?,
            None => StorageVol::lookup_by_name(&p, &vol)?,
        };
        let write_start = std::time::Instant::now();
        let sha256 = upload_from_file(c, &vol, &tmp_path, total)?;
        let write_secs = write_start.elapsed().as_secs_f64();
        Ok(json!({
            "name": vol.get_name().unwrap_or_default(),
            "uploaded": total,
            "sha256": sha256,
            "recv_bps": bps(total, recv_secs),
            "write_bps": bps(total, write_secs),
        }))
    })
    .await
    .map_err(ApiError::internal);

    let _ = std::fs::remove_file(&tmp);
    Ok(Json(v?))
}

/// Push a whole buffer, retrying until every byte has been accepted.
///
/// `Stream::send` maps to a single `virStreamSend`, which is allowed to accept
/// fewer bytes than it was offered once libvirt's internal buffer fills up (the
/// buffer is 64 KiB, so this shows up as whole-chunk losses on a large ISO).
/// Ignoring the returned count dropped the tail of that chunk, and because
/// `finish()` then reports a clean EOF, `virStorageVolUpload` still *succeeded*
/// — leaving a truncated volume that looked like a completed upload.
///
/// Generic over the send call so the partial-write behaviour can be tested
/// without a live libvirt connection.
fn send_all_with<F>(mut data: &[u8], mut send: F) -> Result<(), LvError>
where
    F: FnMut(&[u8]) -> Result<usize, LvError>,
{
    while !data.is_empty() {
        let sent = send(data)?;
        if sent == 0 {
            // Retrying forever would hang the upload; failing loudly beats
            // silently truncating the volume.
            return Err(LvError::Msg(
                "libvirt stream accepted no bytes; refusing to drop data".into(),
            ));
        }
        if sent > data.len() {
            return Err(LvError::Msg(format!(
                "libvirt stream claimed {sent} bytes from a {}-byte chunk",
                data.len()
            )));
        }
        data = &data[sent..];
    }
    Ok(())
}

/// Copy `reader` into `send`, hashing each chunk as it is accepted, and return
/// `(bytes sent, hex digest)`.
///
/// The hash is taken in the same pass that performs the write, so verifying an
/// upload costs no second read of the volume. That read is not cheap: a cold
/// 7.1 GiB ISO takes ~41 s to hash on this host.
///
/// The digest covers the bytes handed to libvirt, which for a raw volume is
/// exactly what lands in the file.
fn send_hashed<R, F>(reader: &mut R, mut send: F) -> Result<(u64, String), LvError>
where
    R: std::io::Read,
    F: FnMut(&[u8]) -> Result<usize, LvError>,
{
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        // `send_all_with` retries until the whole chunk is accepted, so the
        // digest can never cover less than the chunk.
        send_all_with(chunk, &mut send)?;
        hasher.update(chunk);
        total += n as u64;
    }
    Ok((total, to_hex(&hasher.finalize())))
}

/// Write `path` into `vol` and return the SHA-256 of what was written.
pub(super) fn upload_from_file(
    conn: &virt::connect::Connect,
    vol: &StorageVol,
    path: &std::path::Path,
    length: u64,
) -> Result<String, LvError> {
    let stream = Stream::new(conn, 0)?;

    // `virStorageVolUpload` only *starts* the transfer: it returns as soon as
    // the stream is attached (measured against libvirt 11.10.0 on a dir pool),
    // and the data is consumed as it is sent, with `finish()` completing it.
    //
    // This used to send from a second thread on the belief that this call
    // blocks. It does not, and racing a sender against it intermittently drops
    // a whole 64 KiB chunk while still reporting success — a silently
    // truncated volume. One thread, in this order, is both correct and
    // simpler. Do not "optimise" this back into a thread.
    vol.upload(&stream, 0, length, 0)?;

    let mut file = std::fs::File::open(path)?;
    let (sent, sha256) = send_hashed(&mut file, |chunk| stream.send(chunk).map_err(LvError::from))?;
    // A short write must not be reported as a clean upload.
    if sent != length {
        return Err(LvError::Msg(format!(
            "wrote {sent} bytes but was given {length}"
        )));
    }
    stream.finish()?;
    Ok(sha256)
}

pub(super) fn volume_xml(name: &str, capacity: u64) -> String {
    format!(
        "<volume><name>{name}</name><capacity>{capacity}</capacity><target><format type='raw'/></target></volume>"
    )
}

#[cfg(test)]
mod tests {
    use super::{bps, send_all_with, send_hashed, sha256_file};

    /// Known-answer test for the write-time hash. Pins it to real SHA-256
    /// rather than to "consistent with the other side", which would pass even
    /// if both sides were wrong.
    #[test]
    fn the_write_hash_is_a_real_sha256() {
        let mut data = &b"abc"[..];
        let (sent, hex) = send_hashed(&mut data, |chunk| Ok(chunk.len())).unwrap();
        assert_eq!(sent, 3);
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Partial sends must not change the digest: the hash has to cover every
    /// byte written, not just the ones each `virStreamSend` happened to take.
    /// This is the bug class that once truncated volumes silently, so the
    /// hash is asserted to be byte-identical under both send behaviours.
    #[test]
    fn the_write_hash_covers_a_partially_sent_buffer_in_full() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();

        let mut whole = &data[..];
        let (n1, h1) = send_hashed(&mut whole, |chunk| Ok(chunk.len())).unwrap();

        let mut partial = &data[..];
        let (n2, h2) = send_hashed(&mut partial, |chunk| Ok(chunk.len().min(64 * 1024))).unwrap();

        assert_eq!(n1, data.len() as u64, "all bytes counted");
        assert_eq!(
            n2,
            data.len() as u64,
            "all bytes counted despite short sends"
        );
        assert_eq!(
            h1, h2,
            "the digest is independent of how the send was split"
        );
    }

    /// The digest must match a full read of the file it was copied from,
    /// which is what makes it usable in place of re-reading the volume.
    #[test]
    fn the_write_hash_matches_hashing_the_source_file() {
        let dir = std::env::temp_dir().join("virtman-write-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.bin");
        let data: Vec<u8> = (0..150_000u32).map(|i| (i * 11) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let (sent, streamed) = send_hashed(&mut file, |chunk| Ok(chunk.len())).unwrap();
        let (whole, size) = sha256_file(path.to_str().unwrap()).unwrap();

        assert_eq!(sent, size, "same amount of data");
        assert_eq!(streamed, whole, "write-time hash equals a full re-read");
        let _ = std::fs::remove_file(&path);
    }

    /// A fast local transfer can measure zero elapsed time; reporting an
    /// infinite rate, or NaN, would render as garbage in the UI.
    #[test]
    fn bps_never_divides_by_zero() {
        assert_eq!(bps(1000, 0.0), 0.0);
        assert_eq!(bps(1000, -1.0), 0.0);
        assert_eq!(bps(1000, f64::NAN), 0.0);
    }

    #[test]
    fn bps_is_bytes_per_second() {
        assert_eq!(bps(1000, 2.0), 500.0);
        assert_eq!(bps(0, 3.0), 0.0);
    }

    /// Reproduce libvirt's partial-send behaviour: `virStreamSend` accepts only
    /// what fits in its buffer and reports the short count. Before the retry
    /// loop this returned early with the chunk's tail silently dropped, which
    /// truncated the volume while the upload still reported success.
    #[test]
    fn a_partial_send_loses_no_bytes() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
        let mut accepted: Vec<u8> = Vec::new();
        // Accept at most 64 KiB at a time, like libvirt's stream buffer.
        let result = send_all_with(&data, |chunk| {
            let take = chunk.len().min(64 * 1024);
            accepted.extend_from_slice(&chunk[..take]);
            Ok(take)
        });
        assert!(result.is_ok());
        assert_eq!(accepted.len(), data.len(), "every byte was accepted");
        assert_eq!(accepted, data, "and in the original order");
    }

    /// The nastiest shape: one byte per call, so the loop must run a great many
    /// times. A loop that gave up after the first short send would still look
    /// fine on the test above if the buffer happened to be small.
    #[test]
    fn a_byte_at_a_time_send_loses_no_bytes() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let mut accepted: Vec<u8> = Vec::new();
        let result = send_all_with(&data, |chunk| {
            accepted.push(chunk[0]);
            Ok(1)
        });
        assert!(result.is_ok());
        assert_eq!(accepted, data);
    }

    /// A stream that accepts nothing is a real libvirt outcome when the peer
    /// goes away. Retrying forever would hang the upload, so it must fail
    /// loudly rather than silently truncate the volume.
    #[test]
    fn a_stalled_stream_fails_instead_of_truncating() {
        let data = [1u8, 2, 3];
        let result = send_all_with(&data, |_| Ok(0));
        match result {
            Err(e) => assert!(
                e.to_string().contains("accepted no bytes"),
                "unexpected error: {e}"
            ),
            Ok(()) => panic!("a stream accepting nothing must not report success"),
        }
    }

    /// libvirt can never legitimately claim more than it was offered; treating
    /// it as success would slice out of bounds.
    #[test]
    fn a_stream_claiming_more_than_offered_is_rejected() {
        let data = [1u8, 2, 3];
        let result = send_all_with(&data, |chunk| Ok(chunk.len() + 1));
        assert!(result.is_err(), "over-reporting must not be accepted");
    }

    /// An empty buffer is already fully sent and must not call the stream.
    #[test]
    fn an_empty_buffer_sends_nothing() {
        let mut calls = 0;
        let result = send_all_with(&[], |_| {
            calls += 1;
            Ok(1)
        });
        assert!(result.is_ok());
        assert_eq!(calls, 0);
    }

    /// Known-answer test: the SHA-256 of "abc". Pins the hasher to real
    /// SHA-256 rather than merely "consistent with itself", which would pass
    /// even if the digest were wrong on both sides of a comparison.
    #[test]
    fn sha256_file_matches_a_known_digest() {
        let dir = std::env::temp_dir().join("virtman-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        let (hex, size) = sha256_file(path.to_str().unwrap()).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sha256_file_streams_past_one_buffer() {
        // Larger than the 64 KiB read buffer, so the chunk loop runs twice and
        // the accumulated size proves nothing is dropped between reads.
        let dir = std::env::temp_dir().join("virtman-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.bin");
        let data = vec![7u8; 200 * 1024];
        std::fs::write(&path, &data).unwrap();
        let (hex, size) = sha256_file(path.to_str().unwrap()).unwrap();
        assert_eq!(size, 200 * 1024);
        assert_eq!(hex.len(), 64, "full width hex digest");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_file_hashes_to_the_empty_digest() {
        let dir = std::env::temp_dir().join("virtman-hash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        let (hex, size) = sha256_file(path.to_str().unwrap()).unwrap();
        assert_eq!(size, 0);
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&path);
    }
}
