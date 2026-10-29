// Reddit rejects TLS fingerprints older than the latest Chrome, which wreq
// can't imitate. curl-impersonate (https://github.com/lexiforest/curl-impersonate)
// can, so Reddit requests go through it. Its BoringSSL symbols are hidden in the
// shared library, so it can't clash with wreq's.

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString};
use std::ptr;
use std::sync::{LazyLock, Once};
use wreq::header::{HeaderName, HeaderValue};
use wreq::Method;

#[repr(C)]
struct Curl {
	_private: [u8; 0],
}

#[repr(C)]
struct Slist {
	_private: [u8; 0],
}

type Callback = extern "C" fn(*const c_char, usize, usize, *mut c_void) -> usize;

#[link(name = "curl-impersonate")]
extern "C" {
	fn curl_global_init(flags: c_long) -> c_int;
	fn curl_easy_init() -> *mut Curl;
	fn curl_easy_cleanup(handle: *mut Curl);
	fn curl_easy_reset(handle: *mut Curl);
	fn curl_easy_setopt(handle: *mut Curl, option: c_int, ...) -> c_int;
	fn curl_easy_getinfo(handle: *mut Curl, info: c_int, ...) -> c_int;
	fn curl_easy_perform(handle: *mut Curl) -> c_int;
	fn curl_easy_strerror(code: c_int) -> *const c_char;
	fn curl_easy_impersonate(handle: *mut Curl, target: *const c_char, default_headers: c_int) -> c_int;
	fn curl_slist_append(list: *mut Slist, string: *const c_char) -> *mut Slist;
	fn curl_slist_free_all(list: *mut Slist);
}

const CURL_GLOBAL_ALL: c_long = 3;
const CURLOPT_WRITEDATA: c_int = 10001;
const CURLOPT_URL: c_int = 10002;
const CURLOPT_POSTFIELDS: c_int = 10015;
const CURLOPT_HTTPHEADER: c_int = 10023;
const CURLOPT_HEADERDATA: c_int = 10029;
const CURLOPT_CUSTOMREQUEST: c_int = 10036;
const CURLOPT_ACCEPT_ENCODING: c_int = 10102;
const CURLOPT_WRITEFUNCTION: c_int = 20011;
const CURLOPT_HEADERFUNCTION: c_int = 20079;
const CURLOPT_POSTFIELDSIZE_LARGE: c_int = 30120;
const CURLOPT_NOBODY: c_int = 44;
const CURLOPT_POST: c_int = 47;
const CURLOPT_NOSIGNAL: c_int = 99;
const CURLOPT_TIMEOUT_MS: c_int = 155;
const CURLINFO_RESPONSE_CODE: c_int = 0x200000 + 2;

const TIMEOUT_MS: c_long = 30_000;
// Chrome's own value. Setting it is also what makes curl decode the body, which
// Reddit compresses even when unasked because of the app's x-reddit-compression header.
const ACCEPT_ENCODING: &CStr = c"gzip, deflate, br, zstd";

static TARGET: LazyLock<CString> = LazyLock::new(|| {
	let target = std::env::var("REDLIB_IMPERSONATE").ok().filter(|t| !t.is_empty());
	CString::new(target.unwrap_or_else(|| "chrome152".into())).unwrap_or_default()
});

// curl decodes the body, and the response is rebuilt without its framing.
const DROPPED_HEADERS: [&str; 3] = ["content-encoding", "content-length", "transfer-encoding"];

struct Handle(*mut Curl);

impl Drop for Handle {
	fn drop(&mut self) {
		unsafe { curl_easy_cleanup(self.0) }
	}
}

thread_local! {
	// Reusing a handle per thread keeps connections to Reddit open between requests.
	static HANDLE: Handle = {
		static INIT: Once = Once::new();
		INIT.call_once(|| unsafe {
			curl_global_init(CURL_GLOBAL_ALL);
		});
		Handle(unsafe { curl_easy_init() })
	};
}

#[derive(Default)]
struct Received {
	body: Vec<u8>,
	headers: Vec<(String, String)>,
}

extern "C" fn on_body(data: *const c_char, size: usize, count: usize, received: *mut c_void) -> usize {
	let received = unsafe { &mut *received.cast::<Received>() };
	let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size * count) };
	received.body.extend_from_slice(data);
	data.len()
}

extern "C" fn on_header(data: *const c_char, size: usize, count: usize, received: *mut c_void) -> usize {
	let received = unsafe { &mut *received.cast::<Received>() };
	let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size * count) };
	let line = String::from_utf8_lossy(data);
	if line.starts_with("HTTP/") {
		received.headers.clear();
	} else if let Some((name, value)) = line.split_once(':') {
		received.headers.push((name.trim().to_string(), value.trim().to_string()));
	}
	data.len()
}

fn check(code: c_int) -> Result<(), String> {
	if code == 0 {
		return Ok(());
	}
	let message = unsafe { CStr::from_ptr(curl_easy_strerror(code)) };
	Err(format!("curl-impersonate: {}", message.to_string_lossy()))
}

fn c_string(value: impl Into<Vec<u8>>) -> Result<CString, String> {
	CString::new(value).map_err(|_| "Request contains a NUL byte".to_string())
}

/// Blocks until the response arrives, so call it from `spawn_blocking`.
pub fn send(method: &Method, url: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Result<wreq::Response, String> {
	let url = c_string(url)?;
	let lines = headers.iter().map(|(name, value)| c_string(format!("{name}: {value}"))).collect::<Result<Vec<_>, _>>()?;
	let method_name = c_string(method.as_str())?;
	let mut received = Received::default();

	let (status, result) = HANDLE.with(|handle| unsafe {
		let curl = handle.0;
		curl_easy_reset(curl);
		let mut list: *mut Slist = ptr::null_mut();
		for line in &lines {
			list = curl_slist_append(list, line.as_ptr());
		}

		let result = (|| {
			check(curl_easy_impersonate(curl, TARGET.as_ptr(), 1))?;
			check(curl_easy_setopt(curl, CURLOPT_ACCEPT_ENCODING, ACCEPT_ENCODING.as_ptr()))?;
			check(curl_easy_setopt(curl, CURLOPT_URL, url.as_ptr()))?;
			check(curl_easy_setopt(curl, CURLOPT_HTTPHEADER, list))?;
			check(curl_easy_setopt(curl, CURLOPT_NOSIGNAL, 1 as c_long))?;
			check(curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, TIMEOUT_MS))?;
			check(curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, on_body as Callback))?;
			check(curl_easy_setopt(curl, CURLOPT_WRITEDATA, ptr::from_mut(&mut received).cast::<c_void>()))?;
			check(curl_easy_setopt(curl, CURLOPT_HEADERFUNCTION, on_header as Callback))?;
			check(curl_easy_setopt(curl, CURLOPT_HEADERDATA, ptr::from_mut(&mut received).cast::<c_void>()))?;
			match *method {
				Method::GET => {}
				Method::HEAD => check(curl_easy_setopt(curl, CURLOPT_NOBODY, 1 as c_long))?,
				Method::POST => check(curl_easy_setopt(curl, CURLOPT_POST, 1 as c_long))?,
				_ => check(curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, method_name.as_ptr()))?,
			}
			if let Some(body) = body {
				check(curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, body.len() as i64))?;
				check(curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body.as_ptr()))?;
			}
			check(curl_easy_perform(curl))
		})();

		let mut status: c_long = 0;
		curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &mut status as *mut c_long);
		curl_easy_setopt(curl, CURLOPT_HTTPHEADER, ptr::null_mut::<Slist>());
		curl_slist_free_all(list);
		(status, result)
	});
	result?;

	let mut response = http::Response::builder().status(u16::try_from(status).map_err(|_| "Invalid status code")?);
	for (name, value) in received.headers {
		if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
			if !DROPPED_HEADERS.contains(&name.as_str()) {
				response = response.header(name, value);
			}
		}
	}
	let response = response.body(received.body).map_err(|e| e.to_string())?;
	Ok(wreq::Response::from(response))
}
