#!/usr/bin/env python3
"""Stdlib-only S3 (SigV4) client for the sandbox adoption packet.

WHY THIS EXISTS. The §6 gates need a handful of bucket-level operations that
Litestream never performs: reading a bucket's region, putting a lifecycle rule,
listing object versions, reading an object's server-side-encryption metadata,
and deleting every object and the bucket itself. There is no aws CLI, no boto3
and no mc on the sandbox host, and installing one would add a dependency whose
version could change the result. So the operations the packet depends on are
implemented here, in the standard library only, against the published SigV4
algorithm. The same client therefore works against OVH Object Storage and
against any S3-compatible endpoint, which is what makes a rehearsal honest:
the packet can be exercised end to end without an account.

WHY EXPLICIT CREDENTIALS ONLY. Every subcommand takes --access-key-id and
--secret-access-key as arguments and reads nothing else. There is no
credential chain, no ~/.aws, no environment fallback, no EC2 metadata probe.
A gate that proved a restore works without ambient credentials can only be
meaningful if the tool that runs it is itself incapable of finding one, so
this module never grows a credential provider. `no-ambient-credentials`
exists to be tested by reading the source of this file, not by a flag.

USAGE

    s3probe.py --endpoint HOST[:PORT] --bucket B --access-key-id K \\
        --secret-access-key S [--scheme https] [--region R] [--path-style] \\
        <subcommand> [args]

    subcommands:
      probe-bucket        HEAD bucket + GET ?location -> region and reachability
      create-bucket       PUT the bucket (idempotent-ish; 409 means it exists)
      head-object         HEAD one key -> size, etag, SSE metadata, version id
      put-object          PUT a local file as a key (lifecycle aging input)
      get-object          GET a key to stdout or a file
      list-versions       GET ?versions -> [{key, version_id, last_modified}]
      list-objects        GET ?list-type=2 (paginated) -> current keys
      put-lifecycle       PUT ?lifecycle from a JSON rules file
      get-lifecycle       GET ?lifecycle -> the stored rules
      delete-everything   empty the bucket through BOTH ?versions and
                          ?list-type=2, so a service that implements only one
                          of them still gets emptied (alias: delete-all-versions)
      delete-bucket       DELETE the bucket
      probe-capabilities   which bucket-level APIs this endpoint implements

EXIT CODES

    0  the operation succeeded
    1  the S3 service returned an error (its code and message are on stdout as
       JSON, so a gate can assert on the code rather than on an exit status)
    2  usage or local error
"""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import hmac
import json
import os
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from typing import Any, Iterable

# Timeout per request. A sandbox bucket on a metered link is not fast, but a
# hung request would hang a gate, and a gate that hangs produces no evidence.
DEFAULT_TIMEOUT = float(os.environ.get("DG_S3PROBE_TIMEOUT", "30"))

S3_NS = "http://s3.amazonaws.com/doc/2006-03-01/"


# --------------------------------------------------------------------------
# SigV4
# --------------------------------------------------------------------------
def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sign(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def signing_key(secret: str, date_stamp: str, region: str, service: str) -> bytes:
    """The AWS SigV4 derived signing key: HMAC chain, never the secret itself."""
    k_date = _sign(("AWS4" + secret).encode("utf-8"), date_stamp)
    k_region = _sign(k_date, region)
    k_service = _sign(k_region, service)
    return _sign(k_service, "aws4_request")


def canonical_query(pairs: Iterable[tuple[str, str]]) -> str:
    """RFC 3986 encode every name and value, then sort by name then value.

    Sorting must be on the *encoded* strings, which is the detail that silently
    breaks signatures when a query carries more than one parameter.
    """
    encoded = sorted(
        (urllib.parse.quote(str(k), safe="-_.~"), urllib.parse.quote(str(v), safe="-_.~"))
        for k, v in pairs
    )
    return "&".join(f"{k}={v}" for k, v in encoded)


def canonical_uri(path: str) -> str:
    """URI-encode each path segment, preserving '/' separators.

    S3 SigV4 does not double-encode the path, so a key that already contains a
    slash must keep exactly one encoding level. Encoding the whole path with
    safe="/" is the documented behaviour and the one both OVH and MinIO accept.
    """
    return urllib.parse.quote(path, safe="/-_.~")


class S3Error(Exception):
    def __init__(self, status: int, code: str, message: str, body: str = "") -> None:
        super().__init__(f"{status} {code}: {message}")
        self.status = status
        self.code = code
        self.message = message
        self.body = body


class S3Client:
    def __init__(
        self,
        endpoint: str,
        access_key_id: str,
        secret_access_key: str,
        *,
        region: str = "us-east-1",
        scheme: str = "https",
        path_style: bool = True,
        bucket: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        ca_bundle: str | None = None,
    ) -> None:
        if not access_key_id or not secret_access_key:
            # Refused here rather than downstream: an unsigned request that
            # happens to be accepted by a permissive endpoint would be a gate
            # recording a pass it did not earn.
            raise ValueError("both --access-key-id and --secret-access-key are required")
        self.endpoint = endpoint
        self.access_key_id = access_key_id
        self.secret_access_key = secret_access_key
        self.region = region
        self.scheme = scheme
        self.path_style = path_style
        self.bucket = bucket
        self.timeout = timeout
        # A rehearsal endpoint serves TLS with a self-signed certificate, the
        # same shape as a real S3 endpoint. Trusting it explicitly (rather than
        # disabling verification) keeps the rehearsal's TLS behaviour identical
        # to the real run's, so a gate cannot pass here and fail against OVH
        # because the certificate check was switched off.
        self.ca_bundle = ca_bundle
        self._opener: urllib.request.OpenerDirector | None = None
        if ca_bundle:
            if not os.path.exists(ca_bundle):
                raise ValueError(f"--ca-bundle {ca_bundle} does not exist")
            ctx = ssl.create_default_context(cafile=ca_bundle)
            self._opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=ctx))
        # S3 SigV4 signs the host it will connect to, so the two must agree. In
        # virtual-host style the bucket is part of the signed host; in path
        # style it is part of the canonical URI instead.
        self.host = endpoint if path_style or not bucket else f"{bucket}.{endpoint}"

    # -- request construction ------------------------------------------------
    def _url(self, path: str, query: list[tuple[str, str]]) -> str:
        host = self.host if self.path_style else f"{self.bucket}.{self.endpoint}"
        url = f"{self.scheme}://{host}{path}"
        if query:
            url += "?" + canonical_query(query)
        return url

    def request(
        self,
        method: str,
        *,
        path: str | None = None,
        query: list[tuple[str, str]] | None = None,
        body: bytes = b"",
        headers: dict[str, str] | None = None,
        bucket: str | None = None,
        send_host: bool = True,
    ) -> tuple[int, dict[str, str], bytes]:
        if path is None:
            if bucket is None and self.bucket is None:
                raise ValueError("a bucket is required for this operation")
            bucket = bucket or self.bucket
            path = f"/{bucket}" if self.path_style else "/"
        query = query or []
        url_host = self.endpoint if (self.path_style or not bucket) else f"{bucket}.{self.endpoint}"
        url = f"{self.scheme}://{url_host}{path}"
        if query:
            url += "?" + canonical_query(query)

        now = dt.datetime.now(dt.timezone.utc)
        amz_date = now.strftime("%Y%m%dT%H%M%SZ")
        date_stamp = now.strftime("%Y%m%d")
        payload_hash = _sha256_hex(body)

        # Only these three plus whatever the caller adds are signed. The region
        # is used for the credential scope, not sent as a header: a provider
        # that sees a region header it disagrees with can fail a request that
        # would otherwise be correct, and the region is already carried by the
        # scope string the signature is computed over.
        hdrs = {
            "host": url_host,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": amz_date,
        }
        for k, v in (headers or {}).items():
            hdrs[k.lower()] = v

        signed_names = sorted(hdrs)
        canonical_headers = "".join(f"{n}:{_trim(hdrs[n])}\n" for n in signed_names)
        signed_headers = ";".join(signed_names)

        if self.path_style or not bucket:
            canonical_path = path
        else:
            canonical_path = "/"
        canonical_request = "\n".join(
            [
                method,
                canonical_uri(canonical_path),
                canonical_query(query),
                canonical_headers,
                signed_headers,
                payload_hash,
            ]
        )
        scope = f"{date_stamp}/{self.region}/s3/aws4_request"
        string_to_sign = "\n".join(
            ["AWS4-HMAC-SHA256", amz_date, scope, _sha256_hex(canonical_request.encode("utf-8"))]
        )
        signature = hmac.new(
            signing_key(self.secret_access_key, date_stamp, self.region, "s3"),
            string_to_sign.encode("utf-8"),
            hashlib.sha256,
        ).hexdigest()
        hdrs["authorization"] = (
            f"AWS4-HMAC-SHA256 Credential={self.access_key_id}/{scope}, "
            f"SignedHeaders={signed_headers}, Signature={signature}"
        )

        send_headers = {k: v for k, v in hdrs.items() if send_host or k != "host"}
        req = urllib.request.Request(url, data=body if body else None, method=method)
        for k, v in send_headers.items():
            req.add_header(k, v)

        try:
            open_fn = self._opener.open if self._opener is not None else urllib.request.urlopen
            with open_fn(req, timeout=self.timeout) as resp:
                return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read()
        except urllib.error.HTTPError as exc:
            raw = exc.read()
            code, message = _parse_error(raw)
            raise S3Error(exc.code, code, message, raw.decode("utf-8", "replace")) from None
        except urllib.error.URLError as exc:
            raise S3Error(0, "NetworkError", str(exc.reason)) from None


def _trim(value: str) -> str:
    """Collapse internal runs of spaces, as the signing spec requires."""
    return " ".join(value.split())


def _parse_error(raw: bytes) -> tuple[str, str]:
    """S3 error code and message out of an XML error body, or the raw text."""
    try:
        root = ET.fromstring(raw)
    except ET.ParseError:
        text = raw.decode("utf-8", "replace").strip()
        return ("NoSuchBucket" if "NoSuchBucket" in text else "Unknown", text[:400])
    code = root.findtext("Code") or root.findtext(f"{{{S3_NS}}}Code") or "Unknown"
    message = root.findtext("Message") or root.findtext(f"{{{S3_NS}}}Message") or ""
    return code, message[:400]


def _text(el: ET.Element, name: str) -> str:
    return (el.findtext(name) or el.findtext(f"{{{S3_NS}}}{name}") or "").strip()


# --------------------------------------------------------------------------
# Subcommand implementations
# --------------------------------------------------------------------------
def op_probe_bucket(c: S3Client) -> dict[str, Any]:
    """Can we reach the bucket, and what region does it say it is in?

    §6 gate 3 is about an *explicit* endpoint and region. Reading the bucket's
    own region back is how the packet tells a working setting from an ignored
    one, which the rehearsal proved matters: this tool family accepts
    configuration it does not act on without saying so.
    """
    out: dict[str, Any] = {"endpoint": c.endpoint, "scheme": c.scheme, "path_style": c.path_style}
    status, headers, _ = c.request("HEAD", path=f"/{c.bucket}")
    out["bucket"] = c.bucket
    out["bucket_status"] = status
    out["bucket_region_header"] = headers.get("x-amz-bucket-region", "")
    try:
        _, _, body = c.request("GET", path=f"/{c.bucket}", query=[("location", "")])
        out["location"] = _text(ET.fromstring(body), "LocationConstraint")
    except S3Error as exc:
        # A provider that refuses ?location is not a failure to reach the
        # bucket; record it and let the caller decide.
        out["location_error"] = f"{exc.code}: {exc.message}"
    return out


def op_create_bucket(c: S3Client, *, region: str | None = None) -> dict[str, Any]:
    body = b""
    if region and region != "us-east-1":
        root = ET.Element("CreateBucketConfiguration")
        ET.SubElement(root, "LocationConstraint").text = region
        body = ET.tostring(root, encoding="utf-8", xml_declaration=True)
    try:
        status, _, _ = c.request("PUT", path=f"/{c.bucket}", body=body)
        return {"created": True, "status": status}
    except S3Error as exc:
        if exc.code in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
            return {"created": False, "status": exc.status, "code": exc.code}
        raise


def op_head_object(c: S3Client, key: str) -> dict[str, Any]:
    """Object metadata, including the server-side-encryption fields.

    Gate 10 reads this. Encryption that the *bucket* applies is visible here as
    a response header; an encryption setting that only the config claims is
    not visible anywhere, which is the whole reason this subcommand exists.
    """
    status, headers, _ = c.request("HEAD", path=f"/{c.bucket}/{key}")
    return {
        "key": key,
        "status": status,
        "size": int(headers.get("content-length", "0") or 0),
        "etag": headers.get("etag", "").strip('"'),
        "last_modified": headers.get("last-modified", ""),
        "version_id": headers.get("x-amz-version-id", ""),
        "sse": headers.get("x-amz-server-side-encryption", ""),
        "sse_customer_algorithm": headers.get("x-amz-server-side-encryption-customer-algorithm", ""),
        "storage_class": headers.get("x-amz-storage-class", ""),
        "headers": headers,
    }


def op_put_object(c: S3Client, key: str, file_path: str) -> dict[str, Any]:
    with open(file_path, "rb") as fh:
        body = fh.read()
    status, headers, _ = c.request("PUT", path=f"/{c.bucket}/{key}", body=body)
    return {"key": key, "status": status, "bytes": len(body), "etag": headers.get("etag", "").strip('"')}


def op_get_object(c: S3Client, key: str, out_path: str | None) -> dict[str, Any]:
    status, _, body = c.request("GET", path=f"/{c.bucket}/{key}")
    if out_path:
        with open(out_path, "wb") as fh:
            fh.write(body)
    return {"key": key, "status": status, "bytes": len(body), "written": out_path or None}


def op_list_versions(c: S3Client, prefix: str = "") -> dict[str, Any]:
    """Every version of every key, which is what a 7/30-day policy is about.

    A count from LIST (current versions only) would report one object after a
    month of hourly snapshots and would happily pass a 7-day retention check.
    """
    _, _, body = c.request("GET", path=f"/{c.bucket}", query=[("versions", "")])
    root = ET.fromstring(body)
    out = []
    for el in root.findall(f"{{{S3_NS}}}Version") + root.findall("Version"):
        key = _text(el, "Key")
        if prefix and not key.startswith(prefix):
            continue
        out.append(
            {
                "key": key,
                "version_id": _text(el, "VersionId"),
                "last_modified": _text(el, "LastModified"),
                "is_latest": _text(el, "IsLatest") == "true",
            }
        )
    return {"bucket": c.bucket, "count": len(out), "versions": out}


def op_list_objects(c: S3Client, prefix: str = "") -> dict[str, Any]:
    """Current-version keys via ?list-type=2, following truncation to the end.

    This is the second of the two enumerations gate 12 needs, and it exists
    because a service is allowed to implement LIST but not ?versions. An
    endpoint that 501s on ?versions (the rehearsal store does) would leave
    `?versions`-only cleanup deleting nothing while the bucket stays full, and
    the resulting "residue remains" would name the wrong culprit: the packet's
    assumption, not the store's contents.

    Pagination matters for the same reason: a single page is 1000 keys, and a
    packet that deletes one page and calls the rest residue has not emptied
    anything on a bucket with more than 1000 objects.
    """
    keys: list[str] = []
    token = ""
    pages = 0
    while True:
        query: list[tuple[str, str]] = [("list-type", "2")]
        if prefix:
            query.append(("prefix", prefix))
        if token:
            query.append(("continuation-token", token))
        _, _, body = c.request("GET", path=f"/{c.bucket}", query=query)
        root = ET.fromstring(body)
        pages += 1
        for el in root.findall(f"{{{S3_NS}}}Contents") + root.findall("Contents"):
            key = _text(el, "Key")
            if key and (not prefix or key.startswith(prefix)):
                keys.append(key)
        truncated = _text(root, "IsTruncated") == "true"
        token = _text(root, "NextContinuationToken")
        if not truncated or not token:
            break
        if pages > 10_000:  # a service that never stops paginating
            raise S3Error(0, "PaginationLoop", "list-type=2 kept returning truncated pages")
    return {"bucket": c.bucket, "count": len(keys), "pages": pages, "keys": keys}


def op_put_lifecycle(c: S3Client, rules_path: str) -> dict[str, Any]:
    """Store a lifecycle configuration expressed as JSON, translated to the S3 XML.

    The packet expresses rules as JSON so the policy under test is readable and
    diffable in the log; this is the only place the shape is decided.

    The sent rules are echoed back by id. A read-back has to be compared against
    what was sent, and a comparison needs the sent side; keeping it here means
    the packet never has to re-parse its own input file to make the check.
    """
    with open(rules_path, "r", encoding="utf-8") as fh:
        spec = json.load(fh)
    root = ET.Element("LifecycleConfiguration")
    for rule in spec.get("rules", []):
        r = ET.SubElement(root, "Rule")
        ET.SubElement(r, "ID").text = str(rule.get("id", "rule"))
        ET.SubElement(r, "Status").text = str(rule.get("status", "Enabled"))
        if rule.get("prefix"):
            ET.SubElement(r, "Prefix").text = rule["prefix"]
        if rule.get("filter_prefix"):
            f = ET.SubElement(r, "Filter")
            ET.SubElement(f, "Prefix").text = rule["filter_prefix"]
        if "expiration_days" in rule:
            e = ET.SubElement(r, "Expiration")
            ET.SubElement(e, "Days").text = str(rule["expiration_days"])
        if "noncurrent_expiration_days" in rule:
            e = ET.SubElement(r, "NoncurrentVersionExpiration")
            ET.SubElement(e, "NoncurrentDays").text = str(rule["noncurrent_expiration_days"])
    body = ET.tostring(root, encoding="utf-8", xml_declaration=True)
    status, _, _ = c.request("PUT", path=f"/{c.bucket}", query=[("lifecycle", "")], body=body)
    return {
        "status": status,
        "rules": len(spec.get("rules", [])),
        "sent": [
            {
                "id": str(rule.get("id", "rule")),
                "status": str(rule.get("status", "Enabled")),
                "expiration_days": str(rule.get("expiration_days", "")),
                "noncurrent_expiration_days": str(rule.get("noncurrent_expiration_days", "")),
            }
            for rule in spec.get("rules", [])
        ],
    }


def op_get_lifecycle(c: S3Client) -> dict[str, Any]:
    """Read the stored lifecycle configuration back, strictly.

    STRICTNESS IS THE WHOLE POINT. A service that does not implement
    ?lifecycle may answer a GET with something else entirely -- the rehearsal
    store answers GET ?lifecycle with a ListBucketResult, i.e. a plain object
    listing, and it answers 200. A parser that walks the children of whatever
    came back and reads ID/Status/Days out of them finds nothing, and reports
    "N rules" where N is the number of <Contents> elements. That is how the
    first rehearsal run reported "policy stored and read back (30 rules)" and
    "30 rules" was a bucket listing: the PUT had failed with 409
    BucketAlreadyExists and the read-back "confirmed" it against nothing.

    So the root element is checked against the one document this API is
    defined to return. Anything else is reported as an UNIMPLEMENTED
    capability with the element name that actually came back -- never as zero
    rules, which would read as "the policy is gone" and never as "this service
    cannot store one".
    """
    status, _, body = c.request("GET", path=f"/{c.bucket}", query=[("lifecycle", "")])
    try:
        root = ET.fromstring(body)
    except ET.ParseError as exc:
        return {
            "ok": False,
            "status": status,
            "error": {"code": "UnparseableLifecycleResponse", "message": str(exc)},
            "supported": False,
            "count": 0,
            "rules": [],
        }
    name = root.tag.split("}")[-1]
    if name != "LifecycleConfiguration":
        keys = [
            k.text
            for k in root.findall(f"{{{S3_NS}}}Key") + root.findall("Key")
            if k.text
        ]
        return {
            "ok": False,
            "status": status,
            "error": {
                "code": "NotALifecycleConfiguration",
                "message": (
                    f"GET ?lifecycle answered 200 with <{name}>, not a "
                    f"<LifecycleConfiguration>; this service does not implement it"
                    + (f". The document listed {len(keys)} object key(s)." if keys else ".")
                ),
            },
            "supported": False,
            "count": 0,
            "rules": [],
        }
    rules = []
    for r in list(root):
        if r.tag.split("}")[-1] != "Rule":
            continue
        rules.append(
            {
                "id": _text(r, "ID"),
                "status": _text(r, "Status"),
                "expiration_days": _text(r.find("Expiration"), "Days") if r.find("Expiration") is not None else "",
                "noncurrent_expiration_days": _text(
                    r.find("NoncurrentVersionExpiration"), "NoncurrentDays"
                )
                if r.find("NoncurrentVersionExpiration") is not None
                else "",
            }
        )
    return {"ok": True, "status": status, "supported": True, "rules": rules, "count": len(rules)}


def op_delete_everything(c: S3Client) -> dict[str, Any]:
    """Empty the bucket, versions included. Gate 12's residue check starts here.

    Two enumerations, not one, because no single one is guaranteed:

    * `?versions` deletes by key+versionId, which is the ONLY form guaranteed to
      remove noncurrent versions and delete markers. On an unversioned bucket a
      lifecycle rule that has not run yet still leaves them, and a leftover
      noncurrent version is residue even though LIST looks empty.
    * `?list-type=2` catches everything `?versions` cannot see, which is the
      case on a service that answers 501 NotImplemented for `?versions` -- the
      rehearsal store does exactly that, and a `?versions`-only cleanup deleted
      zero objects and then reported "residue remains", naming the store
      instead of the packet's own single-API assumption.

    Residue is therefore re-measured through BOTH. When a service cannot
    enumerate versions at all, that is reported as an unavailable capability
    rather than folded into a zero: the caller can then say what the check did
    and did not cover, which is the only honest form of this answer.
    """
    enumeration: dict[str, str] = {}
    errors: list[dict[str, str]] = []
    deleted = 0
    done_keys: set[str] = set()
    versions_supported = False

    # (1) every version, when the service can enumerate them
    versions: list[dict[str, str]] = []
    try:
        listing = op_list_versions(c)
        versions = listing["versions"]
        versions_supported = True
        enumeration["list-versions"] = "supported"
    except S3Error as exc:
        enumeration["list-versions"] = f"not available ({exc.code})"
    for v in versions:
        try:
            query = [("versionId", v["version_id"])] if v["version_id"] else []
            c.request("DELETE", path=f"/{c.bucket}/{v['key']}", query=query)
            deleted += 1
            done_keys.add(v["key"])
        except S3Error as exc:
            errors.append({"key": v["key"], "code": exc.code, "message": exc.message})

    # (2) every current key, whatever the versions listing could or could not see
    try:
        objects = op_list_objects(c)
        enumeration["list-objects-v2"] = "supported"
    except S3Error as exc:
        enumeration["list-objects-v2"] = f"not available ({exc.code})"
        objects = {"keys": []}
    for key in objects["keys"]:
        if key in done_keys:
            continue
        try:
            c.request("DELETE", path=f"/{c.bucket}/{key}")
            deleted += 1
            done_keys.add(key)
        except S3Error as exc:
            errors.append({"key": key, "code": exc.code, "message": exc.message})

    # (3) re-measure through the same two enumerations
    versions_remaining: int | None
    try:
        versions_remaining = op_list_versions(c)["count"]
    except S3Error as exc:
        versions_remaining = None
        enumeration["list-versions (re-check)"] = f"not available ({exc.code})"
    try:
        objects_remaining = op_list_objects(c)["count"]
    except S3Error as exc:
        objects_remaining = -1
        enumeration["list-objects-v2 (re-check)"] = f"not available ({exc.code})"
    residue = max(versions_remaining or 0, max(objects_remaining, 0))
    return {
        "enumeration": enumeration,
        "versions_found": len(versions) if versions_supported else None,
        "objects_found": len(objects["keys"]),
        "deleted": deleted,
        "errors": errors,
        "versions_remaining": versions_remaining,
        "objects_remaining": objects_remaining,
        "residue_remaining": residue,
    }


def op_delete_bucket(c: S3Client) -> dict[str, Any]:
    try:
        status, _, _ = c.request("DELETE", path=f"/{c.bucket}")
        return {"deleted": True, "status": status}
    except S3Error as exc:
        if exc.code in ("NoSuchBucket",):
            return {"deleted": True, "status": 404, "code": exc.code}
        return {"deleted": False, "status": exc.status, "code": exc.code, "message": exc.message}


# Bucket-level APIs the packet needs beyond plain object PUT/GET/LIST. Not every
# S3-compatible endpoint implements all of them, and the difference matters: a
# target without versioning cannot exercise the 7/30-day retention proof, and a
# target that 501s on ?lifecycle cannot exercise the lifecycle half of it.
# Probing this up front is what lets a gate say "the target cannot answer this"
# instead of recording a FAIL that looks like a defect in the packet.
CAPABILITY_PROBES: dict[str, tuple[str, list[tuple[str, str]]]] = {
    "head-bucket": ("HEAD", []),
    "list-objects-v2": ("GET", [("list-type", "2")]),
    "list-versions": ("GET", [("versions", "")]),
    "get-lifecycle": ("GET", [("lifecycle", "")]),
    "get-bucket-location": ("GET", [("location", "")]),
    "get-tagging": ("GET", [("tagging", "")]),
}


def op_sse_canary(c: S3Client, key_prefix: str) -> dict[str, Any]:
    """Prove whether the service actually honours SSE-C, or only acknowledges it.

    WHY THIS EXISTS. Gate 10 read `x-amz-server-side-encryption` off a HEAD and
    called the stored data encrypted. That is the SERVICE'S CLAIM ABOUT ITSELF,
    and it is not evidence: the rehearsal store answers a HEAD with the
    `x-amz-server-side-encryption-customer-algorithm` the client sent, and a
    direct check showed it storing the object body in PLAINTEXT while echoing
    that header. A gate that reads the echo and passes has proved nothing, and
    it would have passed on OVH too if OVH's behaviour were ever wrong.

    The only client-side observation that separates the two is a READ-BACK
    WITHOUT THE KEY:

    * the GET is refused, naming the encryption -> the bytes at rest are
      ciphertext. This is a proof, and it is what AWS and OVH do for SSE-C.
    * the GET succeeds and returns the exact canary -> the service stored
      plaintext and merely recorded the metadata. A FAIL, not a limitation of
      the test.
    * the GET is refused for an unrelated reason -> INDETERMINATE, and reported
      as such rather than folded into success.

    The same key is then used to read it back, because "the service enforces
    encryption" and "an operator can still restore" are separate properties and
    the second is the one a restore depends on.

    Note the limit of what this can settle. It settles SSE-C. It says nothing
    about SSE-S3/SSE-OMK, where the service decrypts transparently and a
    round-trip succeeds whatever is stored at rest; for those modes the header
    is the only client-visible evidence, and it remains a claim.
    """
    key_b64 = base64.b64encode(os.urandom(32)).decode()
    canary = b"DG-SSE-CANARY-" + os.urandom(16).hex().encode() + b"-PLAINTEXT-MARKER"
    key = f"{key_prefix.rstrip('/')}/{os.urandom(6).hex()}.canary"
    sse_headers = {
        "x-amz-server-side-encryption-customer-algorithm": "AES256",
        "x-amz-server-side-encryption-customer-key": key_b64,
        "x-amz-server-side-encryption-customer-key-md5": base64.b64encode(
            hashlib.md5(base64.b64decode(key_b64)).digest()
        ).decode(),
    }
    out: dict[str, Any] = {"key": key, "canary_bytes": len(canary)}

    try:
        put_status, _, _ = c.request(
            "PUT", path=f"/{c.bucket}/{urllib.parse.quote(key)}", body=canary, headers=sse_headers
        )
        out["put_status"] = put_status
    except S3Error as exc:
        out.update(
            {
                "verdict": "indeterminate",
                "reason": f"the service refused the SSE-C PUT ({exc.code}), so nothing can be said",
            }
        )
        return out

    # 1. read it back with NO key. This is the observation that decides.
    try:
        get_status, _, body = c.request("GET", path=f"/{c.bucket}/{urllib.parse.quote(key)}")
        if canary in body:
            out.update(
                {
                    "verdict": "NOT-ENCRYPTED",
                    "reason": (
                        "the object was read back WITHOUT the customer key and the exact "
                        "plaintext canary came back: the service stored the body in the "
                        "clear while echoing the SSE-C algorithm on HEAD"
                    ),
                    "readback_without_key_status": get_status,
                    "canary_returned_in_clear": True,
                }
            )
            return out
        out.update(
            {
                "verdict": "indeterminate",
                "reason": (
                    f"the read without a key returned {get_status} and {len(body)} bytes that "
                    "do not contain the canary, but the answer is not an encryption refusal, "
                    "so it is not evidence of encryption"
                ),
                "readback_without_key_status": get_status,
            }
        )
    except S3Error as exc:
        # ENFORCED must mean the service refused because of the MISSING KEY, not
        # because the credential is not allowed to read the object at all. A
        # least-privilege policy that denies GetObject on the canary prefix
        # answers AccessDenied too, and calling that proof of encryption would
        # have this gate "prove" a security property on a bucket that merely
        # refuses the read. So the message has to name encryption; a bare
        # access-denied is INDETERMINATE and says which distinction was missing.
        code = (exc.code or "").lower()
        message = (exc.message or "").lower()
        if "encrypt" in message or code in ("invalidargument", "invalidrequest",
                                            "invalidobjectstate", "kmsaccessdenied"):
            out.update(
                {
                    "verdict": "ENFORCED",
                    "reason": (
                        "the read without the customer key was refused for an encryption "
                        f"reason ({exc.code}: {exc.message}), which is the proof that the "
                        "stored bytes are ciphertext rather than the service merely "
                        "recording the header"
                    ),
                    "readback_without_key_status": exc.status,
                }
            )
        else:
            out.update(
                {
                    "verdict": "indeterminate",
                    "reason": (
                        f"the read without a key was refused with {exc.code} -- a reason that "
                        "does not name encryption -- so this is a permission answer, not a "
                        "proof about the bytes. Grant s3:GetObject on the canary prefix and "
                        "re-run to get the real answer"
                    ),
                    "readback_without_key_status": exc.status,
                }
            )
        return out

    # 2. and read it back WITH the key: enforcement and restorability are
    #    separate properties, and a restore depends on the second one.
    try:
        key_status, _, key_body = c.request(
            "GET", path=f"/{c.bucket}/{urllib.parse.quote(key)}", headers=sse_headers
        )
        out["readback_with_key_status"] = key_status
        out["canary_returned_with_key"] = canary in key_body
    except S3Error as exc:
        out["readback_with_key_status"] = exc.status
        out["canary_returned_with_key"] = False
        out["readback_with_key_error"] = f"{exc.code}: {exc.message}"
    return out


def op_probe_capabilities(c: S3Client) -> dict[str, Any]:
    caps: dict[str, str] = {}
    for name, (method, query) in CAPABILITY_PROBES.items():
        try:
            c.request(method, path=f"/{c.bucket}", query=query)
            caps[name] = "supported"
        except S3Error as exc:
            # 501 NotImplemented, and 403 AccessDenied on a read the key may
            # legitimately lack, are both "not demonstrated here" rather than
            # "broken"; the distinction is preserved so the packet can show it.
            if exc.code in ("NotImplemented", "MethodNotAllowed", "AccessDenied", "InvalidRequest"):
                caps[name] = f"not available ({exc.code})"
            else:
                caps[name] = f"error ({exc.code}: {exc.message})"
    return {"bucket": c.bucket, "capabilities": caps}


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------
def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--endpoint", required=True, help="host[:port] of the S3 endpoint")
    p.add_argument("--access-key-id", required=True)
    p.add_argument("--secret-access-key", required=True)
    p.add_argument("--bucket", required=True)
    p.add_argument("--region", default="us-east-1")
    p.add_argument("--scheme", default="https", choices=["http", "https"])
    p.add_argument("--ca-bundle", default=None,
                   help="PEM of the CA to trust for https (the rehearsal endpoint's "
                        "self-signed certificate)")
    p.add_argument("--path-style", dest="path_style", action="store_true", default=True)
    p.add_argument(
        "--virtual-host-style",
        dest="path_style",
        action="store_false",
        help="use bucket.endpoint as the host instead of a path-style bucket",
    )
    p.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)

    sub = p.add_subparsers(dest="command", required=True)
    sub.add_parser("probe-bucket")
    cb = sub.add_parser("create-bucket")
    cb.add_argument("--create-in-region", default=None)
    ho = sub.add_parser("head-object")
    ho.add_argument("key")
    po = sub.add_parser("put-object")
    po.add_argument("key")
    po.add_argument("file")
    go = sub.add_parser("get-object")
    go.add_argument("key")
    go.add_argument("--out", default=None)
    lv = sub.add_parser("list-versions")
    lv.add_argument("--prefix", default="")
    lo = sub.add_parser("list-objects")
    lo.add_argument("--prefix", default="")
    pl = sub.add_parser("put-lifecycle")
    pl.add_argument("rules_json")
    sub.add_parser("get-lifecycle")
    dp = sub.add_parser("delete-everything", aliases=["delete-all-versions"])
    dp.set_defaults(command="delete-everything")
    sub.add_parser("delete-bucket")
    sc = sub.add_parser("sse-canary")
    sc.add_argument("--prefix", default="sbx/sse-canary")
    sub.add_parser("probe-capabilities")

    args = p.parse_args(argv)
    try:
        client = S3Client(
            args.endpoint,
            args.access_key_id,
            args.secret_access_key,
            region=args.region,
            scheme=args.scheme,
            path_style=args.path_style,
            bucket=args.bucket,
            timeout=args.timeout,
            ca_bundle=args.ca_bundle,
        )
    except ValueError as exc:
        # A clean exit 2 with the reason, not a traceback: this is a
        # misconfiguration the operator can act on, and a gate that saw a
        # Python traceback could not tell it apart from a real defect.
        print(f"s3probe: {exc}", file=sys.stderr)
        return 2

    try:
        if args.command == "probe-bucket":
            result = op_probe_bucket(client)
        elif args.command == "create-bucket":
            result = op_create_bucket(client, region=args.create_in_region)
        elif args.command == "head-object":
            result = op_head_object(client, args.key)
        elif args.command == "put-object":
            result = op_put_object(client, args.key, args.file)
        elif args.command == "get-object":
            result = op_get_object(client, args.key, args.out)
        elif args.command == "list-versions":
            result = op_list_versions(client, args.prefix)
        elif args.command == "list-objects":
            result = op_list_objects(client, args.prefix)
        elif args.command == "put-lifecycle":
            result = op_put_lifecycle(client, args.rules_json)
        elif args.command == "get-lifecycle":
            result = op_get_lifecycle(client)
        elif args.command == "delete-everything":
            result = op_delete_everything(client)
        elif args.command == "delete-bucket":
            result = op_delete_bucket(client)
        elif args.command == "sse-canary":
            result = op_sse_canary(client, args.prefix)
        elif args.command == "probe-capabilities":
            result = op_probe_capabilities(client)
        else:  # pragma: no cover - argparse rejects anything else
            raise SystemExit(2)
    except S3Error as exc:
        json.dump({"ok": False, "error": {"status": exc.status, "code": exc.code, "message": exc.message}}, sys.stdout)
        sys.stdout.write("\n")
        return 1
    except (OSError, ValueError, ET.ParseError) as exc:
        json.dump({"ok": False, "error": {"status": -1, "code": type(exc).__name__, "message": str(exc)}}, sys.stdout)
        sys.stdout.write("\n")
        return 2

    json.dump({"ok": True, **result}, sys.stdout, default=str)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
