#!/usr/bin/env python3
"""A validating, logging, fault-injecting S3 front door for the sandbox packet.

THREE JOBS, ONE PROCESS.

1. VALIDATE. Every S3 service the packet can be rehearsed against ignores
   authentication, which means a rehearsal would happily pass with a broken
   signature — the single most embarrassing way for an adoption packet to be
   wrong. This proxy recomputes the SigV4 of every request that arrives,
   checking the access key against the one it was started with, the signed
   header set, and the date window, and answers 403 SignatureDoesNotMatch when
   any of it disagrees. A replication whose config is wrong therefore fails here
   exactly as it would against object storage provider.

2. LOG. Gate 4 asks which S3 operations the pinned sidecar release issues. That is not
   knowable from the replication documentation and not knowable from a single
   quiet run, so every request is recorded as one JSON line: method, bucket,
   key, query, signed headers, status, bytes. The packet reads the log back and
   reports a measured matrix instead of an assumed one.

3. FAIL ON COMMAND. Gate 8 needs revoked credentials and a destination that
   stops accepting writes while the gateway keeps serving. A fault file makes
   that reproducible: write a line like `status=403 code=AccessDenied` into it
   and every matching request gets that response, or `refuse=*` to make the
   endpoint stop answering at all, which is the blocked-egress case.

WHY A PROXY AND NOT A PATCH. The packet has to run unchanged against object storage provider,
where there is no proxy. Everything fault-specific lives here and in the
harness's fault file, never in the code path that talks to the real endpoint.

USAGE

    s3-log-proxy.py --listen 127.0.0.1:PORT --upstream 127.0.0.1:PORT2 \\
        --access-key-id K --log FILE [--faults FILE] [--region R]

The log is JSON Lines. `s3-log-proxy.py --summarize FILE` prints the operation
matrix without starting a server.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import hmac
import json
import os
import re
import socketserver
import ssl
# http.client is used by a module-level helper now (_upstream_conn), so it has
# to be imported at module scope. Left function-local, the helper raised
# NameError inside the request handler and every client saw "remote end closed
# the connection without response" -- which looks exactly like a broken endpoint.
import http.client
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from s3probe import canonical_query, canonical_uri, signing_key  # noqa: E402

# S3's own tolerance for clock skew between client and server.
MAX_SKEW_SECONDS = 15 * 60
LOCK = threading.Lock()
LOG_PATH: str | None = None
UPSTREAM: tuple[str, int] = ("", 0)
UPSTREAM_TLS = False
UPSTREAM_CA: str | None = None
EXPECTED_REGION = "us-east-1"


def _upstream_conn():
    """Open a connection to the upstream endpoint in ITS scheme.

    Hard-coding HTTPConnection here is a trap that only shows up once two
    proxies are chained: the rehearsal endpoint is itself a TLS proxy, so a
    recording proxy in front of it spoke plain HTTP to a TLS port, the
    handshake never completed, and the gate recorded "0 objects written" with
    no error anywhere -- a silent false negative that looks exactly like a
    replication configuration problem.
    """
    if not UPSTREAM_TLS:
        return http.client.HTTPConnection(UPSTREAM[0], UPSTREAM[1], timeout=120)
    ctx = ssl.create_default_context(cafile=UPSTREAM_CA) if UPSTREAM_CA else ssl.create_default_context()
    return http.client.HTTPSConnection(UPSTREAM[0], UPSTREAM[1], timeout=120, context=ctx)
EXPECTED_KEY: str = ""
EXPECTED_SECRET: str = ""
FAULT_PATH: str | None = None
REGION: str = "us-east-1"
COUNTERS = {"requests": 0, "rejected": 0, "faulted": 0, "emulated": 0}


def _read_faults() -> list[dict[str, str]]:
    """The fault file, re-read per request so a gate can change it mid-run.

    Re-reading rather than caching is the point: gate 8 has to break the
    destination, observe the gateway keep serving, and then break egress on the
    same live daemon, which means the fault state has to change underneath a
    running replication.
    """
    if not FAULT_PATH or not os.path.exists(FAULT_PATH):
        return []
    out = []
    try:
        with open(FAULT_PATH, "r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                out.append(_parse_fault_line(line))
    except OSError:
        return []
    return out


def _parse_fault_line(line: str) -> dict[str, str]:
    """`key=value key=value ...`, where a VALUE MAY CONTAIN SPACES.

    Splitting on spaces and keeping only the `=`-bearing tokens silently
    truncated `message=injected revoked-credential fault` to the single word
    "injected", so the daemon logged `AccessDenied: injected` and gate 8's
    check that the log shows the fault it injected -- which greps for the full
    text -- reported zero on every run. That is a counter that can only ever
    read zero: the kind of silent measurement failure a corroborating check
    exists to catch, and which it then failed to catch.

    Two ways to write a value containing spaces, because the format has to say
    which one it is reading:

    * `message="several words"` -- quoted, unambiguous, any position on the line.
    * `message=several words`   -- unquoted, and the LAST pair on the line, so
      the value owns everything after it. Gate 8 writes its messages last.
    """
    fields: dict[str, str] = {}
    rest = line.strip()
    while rest:
        if "=" not in rest:
            break
        name, _, rest = rest.partition("=")
        name = name.strip()
        if rest.startswith('"'):
            value, sep, rest = rest[1:].partition('"')
            fields[name] = value
            rest = rest.strip().lstrip() if sep else ""
            continue
        head, sep, tail = rest.partition(" ")
        if not sep:
            fields[name] = head
            break
        # An unquoted value runs to the end of the line unless the line
        # continues with another `key=`, which is how a message can sit in the
        # middle without swallowing the fields after it.
        nxt = tail.split(" ", 1)[0]
        if "=" in nxt:
            fields[name] = head
            rest = tail.strip()
        else:
            fields[name] = rest.strip()
            break
    return fields


def _fault_for(method: str, bucket: str, key: str) -> dict[str, str] | None:
    for rule in _read_faults():
        if rule.get("method") and rule["method"].upper() != method.upper():
            continue
        if rule.get("bucket") and rule["bucket"] != bucket:
            continue
        if rule.get("key-prefix") and not key.startswith(rule["key-prefix"]):
            continue
        if rule.get("path-contains") and rule["path-contains"] not in key:
            continue
        return rule
    return None


def resign_for_upstream(
    method: str, path: str, query: str, headers: dict[str, str], body: bytes
) -> str:
    """Return an Authorization header valid for the upstream, using our key.

    A proxy that rewrites `host` MUST re-sign. SigV4 covers the host header, so
    forwarding the client's signature verbatim to an endpoint that sees a
    different host produces 403 SignatureDoesNotMatch -- which is what happens
    when this proxy fronts the rehearsal endpoint, which validates signatures
    too. Chaining two validating proxies is only sound if the inner one
    re-signs, and the inner one may: it has just verified the caller holds this
    key, so nothing is being asserted that the client did not already prove.

    The same code path serves a real endpoint, where the rewritten host is
    exactly what the service expects to have been signed.
    """
    host = f"{UPSTREAM[0]}:{UPSTREAM[1]}"
    amz_date = headers.get("x-amz-date") or dt.datetime.now(dt.timezone.utc).strftime(
        "%Y%m%dT%H%M%SZ"
    )
    payload_hash = headers.get("x-amz-content-sha256") or hashlib.sha256(body).hexdigest()
    scoped = dict(headers)
    scoped["host"] = host
    scoped["x-amz-date"] = amz_date
    scoped["x-amz-content-sha256"] = payload_hash

    # Keep the client's region: the upstream validates against the region the
    # client believes it is talking to, and changing it would be inventing a
    # fact rather than re-expressing one.
    region = EXPECTED_REGION
    cred = headers.get("authorization", "")
    parts = dict(
        piece.strip().split("=", 1)
        for piece in cred[len("AWS4-HMAC-SHA256 "):].split(",")
        if "=" in piece
    )
    credential_parts = parts.get("Credential", "").split("/")
    if len(credential_parts) == 5:
        region = credential_parts[2]

    # Sign every x-amz-* header that will be forwarded: SigV4 requires all
    # x-amz-* headers present in a request to be in SignedHeaders, and the
    # forwarder below relays them all. A fixed 3-header set produces
    # "AccessDenied: headers present which were not signed" on any client
    # (boto3, recent AWS SDKs, replication) that sends checksum headers.
    signed_headers = sorted(
        {"host", "x-amz-content-sha256", "x-amz-date"}
        | {h for h in scoped if h.startswith("x-amz-")}
    )
    canonical_headers = "".join(
        f"{h}:{' '.join(scoped[h].split())}\n" for h in signed_headers
    )
    pairs: list[tuple[str, str]] = []
    for part in query.split("&"):
        if not part:
            continue
        name, sep, value = part.partition("=")
        pairs.append(
            (
                urllib.parse.unquote(name.replace("+", " ")),
                urllib.parse.unquote(value.replace("+", " ")) if sep else "",
            )
        )
    canonical_request = "\n".join(
        [
            method,
            canonical_uri(path),
            canonical_query(pairs),
            canonical_headers,
            ";".join(signed_headers),
            payload_hash,
        ]
    )
    date_stamp = amz_date[:8]
    scope = f"{date_stamp}/{region}/s3/aws4_request"
    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
        ]
    )
    signature = hmac.new(
        signing_key(EXPECTED_SECRET, date_stamp, region, "s3"),
        string_to_sign.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    return (
        f"AWS4-HMAC-SHA256 Credential={EXPECTED_KEY}/{scope}, "
        f"SignedHeaders={';'.join(signed_headers)}, Signature={signature}"
    )


LIST_PARAMS = {
    "list-type",
    "prefix",
    "delimiter",
    "continuation-token",
    "start-after",
    "marker",
    "max-keys",
}


def _query_params(query: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for part in query.split("&"):
        if not part:
            continue
        name, sep, value = part.partition("=")
        out[urllib.parse.unquote(name)] = urllib.parse.unquote(value) if sep else ""
    return out


def _needs_empty_listing_emulation(method: str, query: str, payload: bytes, bucket: str) -> bool:
    """True when the upstream's 404 is a store bug and S3 would answer 200.

    gofakes3 answers `404 NoSuchBucket` to a DELIMITER listing of a prefix that
    does not exist, while answering 200-with-no-keys to the same listing without
    a delimiter, and while answering 200 for a missing prefix generally. Real S3
    answers 200 with an empty result in all of those cases.

    It matters because the pinned sidecar release's restore lists
    `<prefix>/generations/` with a delimiter to establish v0.3.x time bounds.
    Against the rehearsal store that 404 is fatal and the restore aborts with
    "get v0.3.x time bounds ... NoSuchBucket" -- so a rehearsal would report that
    restore-from-S3 does not work when it is the store lying, not replication.

    The translation is deliberately narrow: only a 404, only for a GET that is
    itself a listing, only when the error code is NoSuchBucket, and only after
    the upstream confirms the bucket DOES exist. A real NoSuchBucket is passed
    through untouched, because that is a genuine error the packet must see.
    """
    if method != "GET":
        return False
    params = _query_params(query)
    if not (params.keys() & LIST_PARAMS):
        return False
    text = payload.decode("utf-8", "replace")
    if "<Code>NoSuchBucket</Code>" not in text:
        return False
    if not _bucket_exists(bucket):
        return False
    return True


def _bucket_exists(bucket: str) -> bool:
    """Ask the upstream whether the bucket itself is there, not the prefix."""
    try:
        conn = _upstream_conn()
        try:
            import datetime as _dt

            now = _dt.datetime.now(_dt.timezone.utc)
            amz_date = now.strftime("%Y%m%dT%H%M%SZ")
            date_stamp = now.strftime("%Y%m%d")
            path = f"/{bucket}"
            signed = "host;x-amz-content-sha256;x-amz-date"
            canonical_headers = (
                f"host:{UPSTREAM[0]}:{UPSTREAM[1]}\n"
                f"x-amz-content-sha256:UNSIGNED-PAYLOAD\n"
                f"x-amz-date:{amz_date}\n"
            )
            payload_hash = "UNSIGNED-PAYLOAD"
            canonical_request = "\n".join(
                ["GET", canonical_uri(path), "", canonical_headers, signed, payload_hash]
            )
            scope = f"{date_stamp}/{EXPECTED_REGION}/s3/aws4_request"
            sts = "\n".join(
                [
                    "AWS4-HMAC-SHA256",
                    amz_date,
                    scope,
                    hashlib.sha256(canonical_request.encode()).hexdigest(),
                ]
            )
            sig = hmac.new(
                signing_key(EXPECTED_SECRET, date_stamp, EXPECTED_REGION, "s3"),
                sts.encode(),
                hashlib.sha256,
            ).hexdigest()
            conn.request(
                "GET",
                path,
                headers={
                    "host": f"{UPSTREAM[0]}:{UPSTREAM[1]}",
                    "x-amz-date": amz_date,
                    "x-amz-content-sha256": payload_hash,
                    "authorization": (
                        f"AWS4-HMAC-SHA256 Credential={EXPECTED_KEY}/{scope}, "
                        f"SignedHeaders={signed}, Signature={sig}"
                    ),
                },
            )
            return conn.getresponse().status == 200
        finally:
            conn.close()
    except (OSError, http.client.HTTPException):
        return False


def _empty_listing(bucket: str, query: str) -> bytes:
    params = _query_params(query)
    parts = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
        f"<Name>{bucket}</Name>",
        f"<Prefix>{params.get('prefix', '')}</Prefix>",
        "<KeyCount>0</KeyCount>",
        "<MaxKeys>1000</MaxKeys>",
    ]
    if params.get("delimiter"):
        parts.append(f"<Delimiter>{params['delimiter']}</Delimiter>")
    parts.append("<IsTruncated>false</IsTruncated>")
    parts.append("</ListBucketResult>")
    return "".join(parts).encode("utf-8")


def verify_signature(
    method: str, path: str, query: str, headers: dict[str, str], body: bytes
) -> tuple[bool, str]:
    """Recompute the client's SigV4 signature over the request as received.

    A mismatch is reported as 403 SignatureDoesNotMatch, which is what a real
    service answers, so the client under test sees the failure it would see in
    production instead of a proxy-specific error it has never handled.
    """
    auth = headers.get("authorization", "")
    if not auth.startswith("AWS4-HMAC-SHA256 "):
        return False, "MissingSecurityHeader"
    try:
        parts = dict(
            piece.strip().split("=", 1)
            for piece in auth[len("AWS4-HMAC-SHA256 ") :].split(",")
        )
        credential = parts["Credential"]
        signed_headers = parts["SignedHeaders"].split(";")
        provided = parts["Signature"]
    except (KeyError, ValueError):
        return False, "AuthorizationHeaderMalformed"

    credential_parts = credential.split("/")
    if len(credential_parts) != 5:
        return False, "AuthorizationHeaderMalformed"
    key_id, date_stamp, region, service, terminator = credential_parts
    if key_id != EXPECTED_KEY:
        return False, "InvalidAccessKeyId"
    if service != "s3" or terminator != "aws4_request":
        return False, "AuthorizationHeaderMalformed"

    amz_date = headers.get("x-amz-date", "")
    try:
        request_time = dt.datetime.strptime(amz_date, "%Y%m%dT%H%M%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        return False, "AccessDenied"
    if abs((dt.datetime.now(dt.timezone.utc) - request_time).total_seconds()) > MAX_SKEW_SECONDS:
        return False, "RequestTimeTooSkewed"

    # Canonical query: re-parse the raw query string rather than trusting our
    # own idea of the client's encoding, because the client's encoding is
    # exactly what this check is supposed to police. Handles both `k=v` and the
    # bare `k` form that S3 sub-resources are often written in, and keeps
    # duplicate parameters, which is where a naive parser silently loses one.
    pairs: list[tuple[str, str]] = []
    for part in query.split("&"):
        if not part:
            continue
        name, sep, value = part.partition("=")
        pairs.append(
            (
                urllib.parse.unquote(name.replace("+", " ")),
                urllib.parse.unquote(value.replace("+", " ")) if sep else "",
            )
        )

    missing = [h for h in signed_headers if h not in headers]
    if missing:
        return False, f"SignedHeaderMissing:{','.join(missing)}"

    canonical_headers = "".join(
        f"{h}:{' '.join(headers[h].split())}\n" for h in signed_headers
    )
    payload_hash = headers.get("x-amz-content-sha256", hashlib.sha256(body).hexdigest())
    canonical_request = "\n".join(
        [method, canonical_uri(path), canonical_query(pairs), canonical_headers, ";".join(signed_headers), payload_hash]
    )
    scope = f"{date_stamp}/{region}/{service}/aws4_request"
    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
        ]
    )
    expected = hmac.new(
        signing_key(EXPECTED_SECRET, date_stamp, region, service),
        string_to_sign.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    if not hmac.compare_digest(expected, provided):
        return False, "SignatureDoesNotMatch"
    return True, "OK"


def _s3_error(status: int, code: str, message: str, resource: str = "") -> bytes:
    body = (
        f'<?xml version="1.0" encoding="UTF-8"?><Error><Code>{code}</Code>'
        f"<Message>{message}</Message><Resource>{resource}</Resource>"
        f"<RequestId>proxy</RequestId></Error>"
    ).encode("utf-8")
    return body


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "dg-sandbox-s3-proxy/1"

    def log_message(self, *_args: object) -> None:  # noqa: D401 - silence stderr spam
        return

    # -- plumbing ---------------------------------------------------------
    def _read_body(self) -> bytes:
        length = int(self.headers.get("content-length", "0") or 0)
        return self.rfile.read(length) if length else b""

    def _parse(self) -> tuple[str, str, str, str]:
        parsed = urllib.parse.urlsplit(self.path)
        path = urllib.parse.unquote(parsed.path)
        # path style is /<bucket>/<key...>; virtual-host style arrives with the
        # bucket already in the Host, and the path is then only the key.
        parts = path.lstrip("/").split("/", 1)
        bucket = parts[0]
        key = parts[1] if len(parts) > 1 else ""
        return bucket, key, parsed.path, parsed.query

    def _record(self, entry: dict[str, object]) -> None:
        if not LOG_PATH:
            return
        with LOCK:
            with open(LOG_PATH, "a", encoding="utf-8") as fh:
                fh.write(json.dumps(entry, default=str) + "\n")

    def _send(self, status: int, body: bytes, headers: dict[str, str] | None = None) -> None:
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _log_line(
        self,
        bucket: str,
        key: str,
        status: int,
        size: int,
        started: float,
        note: str = "",
        request_bytes: int = 0,
    ) -> None:
        self._record(
            {
                "ts": time.time(),
                "method": self.command,
                "bucket": bucket,
                "key": key,
                "path": urllib.parse.urlsplit(self.path).path,
                "query": urllib.parse.urlsplit(self.path).query,
                "status": status,
                "bytes": size,
                "request_bytes": request_bytes,
                "content_type": self.headers.get("content-type", ""),
                "replication_meta": ";".join(
                    f"{k}={v}" for k, v in self.headers.items() if k.lower().startswith("x-amz-meta-")
                ),
                "host": self.headers.get("host", ""),
                "signed_headers": (
                    dict(
                        p.strip().split("=", 1)
                        for p in self.headers.get("authorization", "")[len("AWS4-HMAC-SHA256 ") :].split(",")
                    ).get("SignedHeaders", "")
                ),
                "sse_requested": self.headers.get("x-amz-server-side-encryption", ""),
                "sse_customer_present": "1" if self.headers.get("x-amz-server-side-encryption-customer-key") else "",
                "elapsed_ms": round((time.time() - started) * 1000, 2),
                "note": note,
            }
        )

    # -- the one code path every method shares ----------------------------
    def setup(self) -> None:
        # Per-connection TLS, in this worker thread. See Server's docstring for
        # why this is not done on the listening socket.
        context = getattr(self.server, "tls_context", None)
        if context is not None:
            self.request = context.wrap_socket(self.request, server_side=True)
        super().setup()
        # A client that opens a connection and then stalls must not hold a
        # worker forever; the packet's own timeouts are what report the failure.
        self.connection.settimeout(60)

    def _handle(self) -> None:
        started = time.time()
        with LOCK:
            COUNTERS["requests"] += 1
        body = self._read_body()
        bucket, key, path, query = self._parse()
        headers = {k.lower(): v for k, v in self.headers.items()}
        status = 0
        note = ""

        fault = _fault_for(self.command, bucket, key)
        if fault and fault.get("refuse"):
            # The blocked-egress case: the destination simply stops answering.
            # Closing without a response is what a blackholed route looks like
            # to the client, and it is deliberately a different failure mode
            # from an error response, because a gateway that only handles the
            # error case can still lose writes silently.
            with LOCK:
                COUNTERS["faulted"] += 1
            self._log_line(bucket, key, 0, 0, started, note=f"fault:refuse {fault}", request_bytes=len(body))
            try:
                self.connection.close()
            except OSError:
                pass
            self.close_connection = True
            return

        if not fault:
            ok, code = verify_signature(self.command, path, query, headers, body)
            if not ok:
                with LOCK:
                    COUNTERS["rejected"] += 1
                payload = _s3_error(403, code, f"the request signature the proxy recomputed did not match: {code}")
                status = 403
                note = f"rejected:{code}"
                self._send(403, payload, {"Content-Type": "application/xml"})
                self._log_line(bucket, key, status, len(payload), started, note=note, request_bytes=len(body))
                return

        # Forward upstream over a fresh connection, with the Host rewritten to
        # the upstream. The client signed the host it dialed, which is this
        # proxy; the upstream sees its own name, which is what it expects.
        conn = _upstream_conn()
        up_headers = {k: v for k, v in headers.items() if k not in ("host", "authorization", "accept-encoding")}
        up_headers["host"] = f"{UPSTREAM[0]}:{UPSTREAM[1]}"
        up_headers["content-length"] = str(len(body))
        if headers.get("authorization"):
            up_headers["authorization"] = resign_for_upstream(
                self.command, path, query, headers, body
            )
        if fault and fault.get("status"):
            status = int(fault["status"])
            payload = _s3_error(
                status,
                fault.get("code", "AccessDenied"),
                fault.get("message", "injected by the sandbox fault file"),
                key,
            )
            with LOCK:
                COUNTERS["faulted"] += 1
            note = f"fault:{status}"
            self._send(status, payload, {"Content-Type": "application/xml"})
            self._log_line(bucket, key, status, len(payload), started, note=note, request_bytes=len(body))
            return
        try:
            conn.request(self.command, self.path, body=body, headers=up_headers)
            resp = conn.getresponse()
            payload = resp.read()
            status = resp.status
            out_headers = {k: v for k, v in resp.getheaders() if k.lower() not in ("content-length", "transfer-encoding")}
            if status == 404 and _needs_empty_listing_emulation(self.command, query, payload, bucket):
                payload = _empty_listing(bucket, query)
                status = 200
                out_headers = {"Content-Type": "application/xml"}
                note = "emulated:missing-prefix-listing"
                with LOCK:
                    COUNTERS["emulated"] += 1
            self._send(status, payload, out_headers)
        except (OSError, http.client.HTTPException) as exc:
            payload = _s3_error(502, "BadGateway", f"upstream {UPSTREAM[0]}:{UPSTREAM[1]} failed: {exc}")
            status = 502
            note = f"upstream-error:{exc}"
            self._send(502, payload, {"Content-Type": "application/xml"})
        finally:
            conn.close()
        self._log_line(bucket, key, status, len(payload), started, note=note, request_bytes=len(body))

    do_GET = do_PUT = do_HEAD = do_POST = do_DELETE = _handle
    do_PUTOBJ = _handle


class Server(socketserver.ThreadingTCPServer):
    """A plain server that can hand each connection a TLS handshake to do.

    The handshake is done by the worker thread, not on the listening socket.
    Wrapping the LISTENING socket makes accept() perform the handshake, which
    means one connection that opens and sends nothing -- which replication does,
    and which every connection pool does -- blocks the accept loop, so the real
    request behind it is never served. The symptom is a client that connects,
    sits there, and never gets a byte, with nothing logged anywhere.
    """

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, handler, certfile: str | None = None, keyfile: str | None = None):
        # Set before super().__init__, which calls server_bind: the context has
        # to exist by the time the socket is bound, or the workers find TLS off.
        self.certfile = certfile
        self.keyfile = keyfile
        super().__init__(addr, handler)

    def server_bind(self):
        super().server_bind()
        if self.certfile:
            # Per-SERVER, not per-process: a worker thread reads it back through
            # self.server, so two servers in one process (or a module loaded
            # twice) cannot hand a connection the wrong context.
            self.tls_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            self.tls_context.load_cert_chain(self.certfile, self.keyfile)
        else:
            self.tls_context = None


def summarize(path: str) -> int:
    """Print the operation matrix the packet's gate 4 reports.

    Counted by (method, bucket-present, key pattern) because that is the shape
    a credential policy is written in: which verbs, against which prefixes. A
    raw request dump is evidence; this table is what an owner can act on.
    """
    ops: dict[str, int] = {}
    total = 0
    errors: dict[str, int] = {}
    by_bucket: dict[str, int] = {}
    sse_seen = 0
    uploaded = 0
    generations: set[str] = set()
    try:
        with open(path, "r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                total += 1
                status = int(entry.get("status") or 0)
                if status >= 400 or entry.get("note"):
                    errors[f"{entry.get('method')} {status}"] = errors.get(f"{entry.get('method')} {status}", 0) + 1
                if entry.get("sse_requested") or entry.get("sse_customer_present"):
                    sse_seen += 1
                if entry.get("method") == "PUT":
                    uploaded += int(entry.get("request_bytes") or 0)
                for part in (entry.get("query") or "").split("&"):
                    if part.startswith("prefix="):
                        generations.add(part)
                bucket = entry.get("bucket") or "(none)"
                prefix = (entry.get("key") or "").split("/")[0] or "(bucket-level)"
                label = f"{entry.get('method')} {bucket}/{prefix}*"
                ops[label] = ops.get(label, 0) + 1
                by_bucket[bucket] = by_bucket.get(bucket, 0) + 1
    except OSError as exc:
        print(f"cannot read {path}: {exc}", file=sys.stderr)
        return 2

    print(f"requests: {total}")
    for label in sorted(ops):
        print(f"  {ops[label]:5d}  {label}")
    if errors:
        print("non-2xx:")
        for label in sorted(errors):
            print(f"  {errors[label]:5d}  {label}")
    print(f"bytes uploaded: {uploaded}")
    if generations:
        print(f"distinct ListObjectsV2 prefixes probed: {len(generations)}")
    if sse_seen:
        print(f"requests carrying server-side-encryption headers, in THIS cycle's log: {sse_seen}")
    else:
        # Scoped to this window, on purpose. An unqualified "replication sent
        # none" reads as a statement about the whole run, and gate 10's SSE-C
        # probe -- run through a different proxy, later, on purpose -- does sign
        # those headers. Two lines in one packet, contradicting each other
        # because one of them failed to say what it was talking about.
        print("requests carrying server-side-encryption headers, in THIS cycle's log: 0 "
              "(no SSE-C or SSE header in the requests proxied for this window; gate 10's "
              "probe runs through its own proxy and is reported there)")
    return 0


def main(argv: list[str] | None = None) -> int:
    global LOG_PATH, UPSTREAM, EXPECTED_KEY, EXPECTED_SECRET, FAULT_PATH, UPSTREAM_TLS, UPSTREAM_CA, EXPECTED_REGION
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--listen", default="127.0.0.1:0")
    p.add_argument("--upstream", required=True)
    p.add_argument("--access-key-id", required=True)
    p.add_argument("--secret-access-key", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--faults", default=None)
    p.add_argument("--region", default="us-east-1")
    p.add_argument("--print-listen", action="store_true",
                   help="write the bound port to stdout and then serve. The caller must "
                        "background this process; a command substitution around it waits "
                        "for a process that never exits. Use --port-file instead")
    p.add_argument("--port-file", default=None,
                   help="write the bound port to this file, then serve. The race-free "
                        "way to learn a port chosen by the OS: wait for the file to "
                        "be non-empty instead of capturing a command substitution")
    p.add_argument("--tls-cert", default=None, help="serve https with this certificate")
    p.add_argument("--tls-key", default=None, help="the key for --tls-cert")
    p.add_argument("--upstream-scheme", default="http", choices=["http", "https"],
                   help="the scheme the UPSTREAM speaks. Set https when the upstream is "
                        "itself a TLS endpoint, e.g. the rehearsal endpoint's proxy")
    p.add_argument("--upstream-ca-bundle", default=None,
                   help="CA to trust for an https upstream")
    p.add_argument("--summarize", default=None, help="print the operation matrix for a log and exit")
    args = p.parse_args(argv)

    if args.summarize:
        return summarize(args.summarize)

    if not args.access_key_id or not args.secret_access_key:
        print("s3-log-proxy: a fixed key pair is required; the proxy validates signatures", file=sys.stderr)
        return 2

    host, _, port = args.listen.rpartition(":")
    up_host, sep, up_port = args.upstream.rpartition(":")
    if not sep:
        # Bare host (the packet's documented DG_SANDBOX_ENDPOINT shape).
        # Default the port from the scheme instead of crashing on int(host).
        up_host, up_port = args.upstream, 443 if args.upstream_scheme == "https" else 80
    else:
        up_port = int(up_port)
    UPSTREAM = (up_host, up_port)
    UPSTREAM_TLS = args.upstream_scheme == "https"
    UPSTREAM_CA = args.upstream_ca_bundle
    EXPECTED_KEY = args.access_key_id
    EXPECTED_SECRET = args.secret_access_key
    LOG_PATH = args.log
    FAULT_PATH = args.faults
    REGION = args.region
    EXPECTED_REGION = args.region

    if bool(args.tls_cert) != bool(args.tls_key):
        print("s3-log-proxy: --tls-cert and --tls-key go together", file=sys.stderr)
        return 2
    server = Server(
        (host or "127.0.0.1", int(port)),
        Handler,
        args.tls_cert,
        args.tls_key,
    )
    bound = server.server_address[1]
    # Announce the port BEFORE serving, through a channel that does not depend
    # on the caller's pipe lifetime. `pport="$(proxy --print-listen | head -1)"`
    # deadlocks: head exits after one line, but the proxy keeps the pipe's write
    # end open while it serves, so the substitution waits forever. This is not a
    # theoretical hazard -- it is what the first two rehearsal runs hung on, and
    # it would have hung the real object storage provider run identically.
    if args.port_file:
        # Write-then-rename so a reader never sees a half-written port.
        tmp = f"{args.port_file}.tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            fh.write(f"127.0.0.1:{bound}\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, args.port_file)
    if args.print_listen:
        print(f"127.0.0.1:{bound}", flush=True)
        # Nothing more is written to stdout, so a caller that does background this
        # and redirects stdout gets a clean stream with no log noise on it.
        try:
            sys.stdout.close()
        except Exception:
            pass
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        with LOCK:
            summary_line = json.dumps(COUNTERS)
        with open(LOG_PATH + ".summary", "w", encoding="utf-8") as fh:
            fh.write(summary_line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
