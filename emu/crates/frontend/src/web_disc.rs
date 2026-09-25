//! Disc images read from the browser a chunk at a time.
//!
//! The web build used to copy a whole disc into wasm memory before booting
//! it: 700 MB for a full CD, and wasm memory never shrinks. Here the image
//! stays where it is -- a picked `File` (the browser reads it from disk), or
//! a same-origin URL read with HTTP range requests -- and the emulator keeps
//! only a few megabytes of it:
//!
//! - one pass at open hashes the image (input tapes and saves key on it) and
//!   keeps bytes 12..20 of every sector, the header the drive reports after a
//!   seek, so a seek answers without waiting for its sector;
//! - sector reads come from a small cache of 32-sector chunks, filled by
//!   asynchronous `Blob.slice` / range reads.
//!
//! Browser reads are asynchronous and the emulator runs synchronously inside
//! a frame, so the frontend asks the drive for its upcoming sectors before
//! each frame (`CdRom::prefetch_upcoming` in the emulator core) and holds
//! the frame back until they are here. See that function for why this keeps the
//! drive from ever waiting.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};

use psx_iso::{TrackSource, SECTOR_BYTES};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Sectors per cached chunk.
const CHUNK_SECTORS: u64 = 32;
const CHUNK_BYTES: u64 = CHUNK_SECTORS * SECTOR_BYTES as u64;
/// Chunks kept beyond the pinned boot set: about 3.6 MB.
const CACHED_CHUNKS: usize = 48;
/// Largest single read request, in chunks.
const MAX_REQUEST_CHUNKS: u64 = 16;
/// Bytes per read while scanning at open (a whole number of sectors).
const SCAN_BYTES: u64 = 892 * SECTOR_BYTES as u64;

#[wasm_bindgen(inline_js = r#"
const _img = new Map();
function _read(s, start, end) {
  if (s.blob) return s.blob.slice(start, end).arrayBuffer();
  return fetch(s.url, { headers: { Range: 'bytes=' + start + '-' + (end - 1) } }).then((r) => {
    if (r.status !== 206) throw new Error(s.url + ': HTTP ' + r.status + ' to a range request');
    return r.arrayBuffer();
  });
}
export function wdOpenBlob(id, blob) { _img.set(id, { blob, url: null, done: [] }); return blob.size; }
export async function wdOpenUrl(id, url) {
  const r = await fetch(url, { headers: { Range: 'bytes=0-0' } });
  if (r.status !== 206) return -1;
  const range = r.headers.get('content-range') || '';
  const total = Number(range.split('/')[1]);
  if (!(total > 0)) return -1;
  _img.set(id, { blob: null, url, done: [] });
  return total;
}
export function wdClose(id) { _img.delete(id); }
export function wdRead(id, start, end) {
  const s = _img.get(id);
  if (!s) return;
  _read(s, start, end).then(
    (b) => { s.done.push(start, new Uint8Array(b)); },
    (e) => { console.error('[psoxide] disc read', start, end, e); s.done.push(start, null); });
}
export function wdTake(id) {
  const s = _img.get(id);
  if (!s || s.done.length === 0) return null;
  const d = s.done;
  s.done = [];
  return d;
}
export async function wdReadNow(id, start, end) {
  const s = _img.get(id);
  if (!s) throw new Error('disc closed');
  return new Uint8Array(await _read(s, start, end));
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = wdOpenBlob)]
    fn wd_open_blob(id: u32, blob: &web_sys::Blob) -> f64;
    #[wasm_bindgen(js_name = wdOpenUrl)]
    fn wd_open_url(id: u32, url: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = wdClose)]
    fn wd_close(id: u32);
    #[wasm_bindgen(js_name = wdRead)]
    fn wd_read(id: u32, start: f64, end: f64);
    #[wasm_bindgen(js_name = wdTake)]
    fn wd_take(id: u32) -> JsValue;
    #[wasm_bindgen(js_name = wdReadNow)]
    fn wd_read_now(id: u32, start: f64, end: f64) -> js_sys::Promise;
}

struct Image {
    len: u64,
    chunks: HashMap<u64, Box<[u8]>>,
    /// Least recently used first. Pinned chunks are not in here.
    order: VecDeque<u64>,
    pinned: HashSet<u64>,
    in_flight: HashSet<u64>,
    /// Bytes 12..20 of every whole sector, from the scan at open.
    headers: Vec<[u8; 8]>,
}

impl Image {
    fn touch(&mut self, chunk: u64) {
        if self.pinned.contains(&chunk) {
            return;
        }
        if let Some(i) = self.order.iter().position(|&c| c == chunk) {
            self.order.remove(i);
        }
        self.order.push_back(chunk);
    }

    fn insert(&mut self, chunk: u64, bytes: Box<[u8]>) {
        self.chunks.insert(chunk, bytes);
        self.touch(chunk);
        while self.order.len() > CACHED_CHUNKS {
            if let Some(old) = self.order.pop_front() {
                self.chunks.remove(&old);
            }
        }
    }

    fn chunk_range(&self, offset: u64, len: u64) -> std::ops::RangeInclusive<u64> {
        let end = (offset + len.max(1)).min(self.len.max(1)) - 1;
        offset / CHUNK_BYTES..=end / CHUNK_BYTES
    }

    fn resident(&mut self, offset: u64, len: u64) -> bool {
        let range = self.chunk_range(offset, len);
        if !range.clone().all(|c| self.chunks.contains_key(&c)) {
            return false;
        }
        for c in range {
            self.touch(c);
        }
        true
    }

    /// Ask for every chunk of the range that is neither here nor on its way,
    /// in runs of adjacent chunks.
    fn request(&mut self, id: u32, offset: u64, len: u64) {
        let mut run: Option<(u64, u64)> = None;
        let flush = |run: (u64, u64), image: &Image| {
            let start = run.0 * CHUNK_BYTES;
            let end = ((run.1 + 1) * CHUNK_BYTES).min(image.len);
            wd_read(id, start as f64, end as f64);
        };
        for c in self.chunk_range(offset, len) {
            let wanted = !self.chunks.contains_key(&c) && !self.in_flight.contains(&c);
            if wanted {
                self.in_flight.insert(c);
                run = match run {
                    Some((first, last)) if last + 1 == c && c - first < MAX_REQUEST_CHUNKS => {
                        Some((first, c))
                    }
                    Some(done) => {
                        flush(done, self);
                        Some((c, c))
                    }
                    None => Some((c, c)),
                };
            } else if let Some(done) = run.take() {
                flush(done, self);
            }
        }
        if let Some(done) = run {
            flush(done, self);
        }
    }

    /// Move finished browser reads into the cache.
    fn pump(&mut self, id: u32) {
        let done = wd_take(id);
        if done.is_null() {
            return;
        }
        let done = js_sys::Array::from(&done);
        let mut i = 0;
        while i + 1 < done.length() {
            let start = done.get(i).as_f64().unwrap_or(0.0) as u64;
            let bytes = done.get(i + 1);
            i += 2;
            let first = start / CHUNK_BYTES;
            if bytes.is_null() {
                // A failed read: forget it so the next request retries.
                self.in_flight
                    .retain(|&c| c < first || c >= first + MAX_REQUEST_CHUNKS);
                continue;
            }
            let bytes = js_sys::Uint8Array::new(&bytes);
            let total = u64::from(bytes.length());
            let mut at = 0u64;
            let mut chunk = first;
            while at < total {
                let n = CHUNK_BYTES.min(total - at);
                let mut buf = vec![0u8; n as usize].into_boxed_slice();
                bytes.subarray(at as u32, (at + n) as u32).copy_to(&mut buf);
                self.in_flight.remove(&chunk);
                self.insert(chunk, buf);
                at += n;
                chunk += 1;
            }
        }
    }

    fn read(&mut self, offset: u64, out: &mut [u8]) -> bool {
        let len = out.len() as u64;
        if !self.resident(offset, len) {
            return false;
        }
        let mut done = 0u64;
        while done < len {
            let at = offset + done;
            let chunk = at / CHUNK_BYTES;
            let within = at - chunk * CHUNK_BYTES;
            let bytes = &self.chunks[&chunk];
            let n = (len - done).min(bytes.len() as u64 - within);
            out[done as usize..(done + n) as usize]
                .copy_from_slice(&bytes[within as usize..(within + n) as usize]);
            done += n;
        }
        true
    }

    /// Bytes 12..20 of a sector, or a part of them, from the scan index.
    fn header(&self, offset: u64, out: &mut [u8]) -> bool {
        let sector = offset / SECTOR_BYTES as u64;
        let within = (offset % SECTOR_BYTES as u64) as usize;
        let Some(header) = self.headers.get(sector as usize) else {
            return false;
        };
        if within < 12 || within + out.len() > 20 {
            return false;
        }
        out.copy_from_slice(&header[within - 12..within - 12 + out.len()]);
        true
    }
}

thread_local! {
    static IMAGES: RefCell<HashMap<u32, Image>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
}

fn with_image<R>(id: u32, f: impl FnOnce(&mut Image) -> R) -> Option<R> {
    IMAGES.with(|images| {
        images.borrow_mut().get_mut(&id).map(|image| {
            image.pump(id);
            f(image)
        })
    })
}

/// A disc image in the browser, read on demand. Plain data (an id into this
/// thread's image table), so it satisfies the `Send + Sync` a track source
/// needs; wasm here is single-threaded.
pub struct WebImage {
    id: u32,
    len: u64,
}

impl WebImage {
    /// Table id, for [`busy`] and [`pin_resident`].
    pub fn id(&self) -> u32 {
        self.id
    }
}

impl TrackSource for WebImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        if offset
            .checked_add(out.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return false;
        }
        with_image(self.id, |image| {
            if image.read(offset, out) || image.header(offset, out) {
                return true;
            }
            image.request(self.id, offset, out.len() as u64);
            false
        })
        .unwrap_or(false)
    }

    fn ready(&self, offset: u64, len: u64) -> bool {
        with_image(self.id, |image| image.resident(offset, len)).unwrap_or(false)
    }

    fn prefetch(&self, offset: u64, len: u64) {
        let len = len.min(self.len.saturating_sub(offset));
        if len > 0 {
            with_image(self.id, |image| image.request(self.id, offset, len));
        }
    }
}

impl Drop for WebImage {
    fn drop(&mut self) {
        IMAGES.with(|images| images.borrow_mut().remove(&self.id));
        wd_close(self.id);
    }
}

/// Whether image `id` still has reads in flight (after taking finished ones).
pub fn busy(id: u32) -> bool {
    with_image(id, |image| !image.in_flight.is_empty()).unwrap_or(false)
}

/// Keep every chunk now in the cache for good. Called once a boot succeeds,
/// so the boot files (volume descriptor, directories, SYSTEM.CNF, the
/// executable) stay in hand for a reboot without another round of reads.
pub fn pin_resident(id: u32) {
    with_image(id, |image| {
        let resident: Vec<u64> = image.chunks.keys().copied().collect();
        image.pinned.extend(resident);
        image.order.retain(|c| !image.pinned.contains(c));
    });
}

/// An opened image plus what the scan learned.
pub struct Opened {
    pub image: WebImage,
    /// [`emulator_core::game_image_hash`] of the whole image.
    pub hash: u64,
}

/// Open a picked file.
pub async fn open_blob(blob: &web_sys::Blob) -> Result<Opened, String> {
    let id = next_id();
    let len = wd_open_blob(id, blob) as u64;
    scan(id, len).await
}

/// Open a same-origin URL read with range requests. `Ok(None)` when the
/// server does not answer range requests, so the caller can fall back to a
/// plain download.
pub async fn open_url(url: &str) -> Result<Option<Opened>, String> {
    let id = next_id();
    let len = JsFuture::from(wd_open_url(id, url))
        .await
        .map_err(js_error)?
        .as_f64()
        .unwrap_or(-1.0);
    if len <= 0.0 {
        return Ok(None);
    }
    scan(id, len as u64).await.map(Some)
}

fn next_id() -> u32 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1).max(1));
        id
    })
}

/// Read the image once, front to back: hash it, index the sector headers,
/// and keep its first chunk (the disc model reads sector 0 at mount).
async fn scan(id: u32, len: u64) -> Result<Opened, String> {
    let mut image = Image {
        len,
        chunks: HashMap::new(),
        order: VecDeque::new(),
        pinned: HashSet::new(),
        in_flight: HashSet::new(),
        headers: Vec::new(),
    };
    image
        .headers
        .try_reserve_exact((len / SECTOR_BYTES as u64) as usize)
        .map_err(|_| "not enough memory for the disc index".to_string())?;
    let mut hasher = emulator_core::GameImageHasher::new();
    let mut buf = Vec::new();
    let mut at = 0u64;
    while at < len {
        let end = (at + SCAN_BYTES).min(len);
        let piece = JsFuture::from(wd_read_now(id, at as f64, end as f64))
            .await
            .map_err(js_error)?;
        let piece = js_sys::Uint8Array::new(&piece);
        if u64::from(piece.length()) != end - at {
            wd_close(id);
            return Err("file changed while it was being read".to_string());
        }
        buf.resize(piece.length() as usize, 0);
        piece.copy_to(&mut buf);
        hasher.update(&buf);
        for sector in buf.chunks_exact(SECTOR_BYTES) {
            let mut header = [0u8; 8];
            header.copy_from_slice(&sector[12..20]);
            image.headers.push(header);
        }
        if at == 0 {
            let n = (CHUNK_BYTES as usize).min(buf.len());
            image.insert(0, buf[..n].to_vec().into_boxed_slice());
            image.pinned.insert(0);
            image.order.retain(|&c| c != 0);
        }
        at = end;
    }
    IMAGES.with(|images| images.borrow_mut().insert(id, image));
    Ok(Opened {
        image: WebImage { id, len },
        hash: hasher.finish(),
    })
}

fn js_error(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
