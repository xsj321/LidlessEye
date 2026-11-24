#!/usr/bin/env python3
"""Minimal S3-compatible HTTP server for testing DuckDB httpfs.

Implements enough of the S3 REST API for DuckDB's httpfs extension, including
the multipart upload sequence that every `COPY ... TO 's3://...'` uses:
  - POST   /bucket/key?uploads                -> CreateMultipartUpload
  - PUT    /bucket/key?partNumber=N&uploadId= -> UploadPart
  - POST   /bucket/key?uploadId=X             -> CompleteMultipartUpload
  - DELETE /bucket/key?uploadId=X             -> AbortMultipartUpload
  - PUT    /bucket/key                        -> store object (single PUT)
  - GET    /bucket/key                        -> read object (with Range support)
  - HEAD   /bucket/key                        -> object metadata
  - DELETE /bucket/key                        -> remove object
  - GET    /bucket?list-type=2&prefix=...     -> ListObjectsV2 (XML)

Path-style addressing only. Authentication headers are ignored. Objects are
stored as plain files under a root directory; in-flight parts live under a
separate `.multipart` directory and are never returned by object listing.
"""
import hashlib
import html
import os
import re
import shutil
import sys
import urllib.parse
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[2] if len(sys.argv) > 2 else "/tmp/s3mock-data"
MULTIPART_ROOT = os.path.join(ROOT, ".multipart")
os.makedirs(ROOT, exist_ok=True)
os.makedirs(MULTIPART_ROOT, exist_ok=True)

NS = "http://s3.amazonaws.com/doc/2006-03-01/"


def obj_path(bucket, key):
    d = os.path.join(ROOT, bucket)
    os.makedirs(d, exist_ok=True)
    # Preserve slashes in keys as directories, but guard traversal.
    safe = key.lstrip("/").replace("..", "__")
    return os.path.join(d, safe)


def upload_dir(bucket, key, upload_id):
    # Upload ids are opaque and may contain characters that are illegal in
    # file names. Hashing the triple also keeps one tenant's parts isolated.
    digest = hashlib.sha256(f"{bucket}\0{key}\0{upload_id}".encode()).hexdigest()
    d = os.path.join(MULTIPART_ROOT, digest)
    os.makedirs(d, exist_ok=True)
    return d


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("s3mock: " + (fmt % args) + "\n")

    def _bucket_key(self):
        path = urllib.parse.urlsplit(self.path).path.lstrip("/")
        parts = path.split("/", 1)
        bucket = parts[0]
        key = parts[1] if len(parts) > 1 else ""
        return bucket, key

    def _query(self):
        # DuckDB sends `?uploads=` with an empty value; without
        # keep_blank_values the flag disappears and the request looks like a
        # malformed CompleteMultipartUpload.
        return urllib.parse.parse_qs(
            urllib.parse.urlsplit(self.path).query, keep_blank_values=True
        )

    def _drain_body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if length else b""

    def _send_xml(self, status, body):
        self.send_response(status)
        self.send_header("Content-Type", "application/xml")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_empty(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        bucket, key = self._bucket_key()
        q = self._query()
        self._drain_body()

        if "uploads" in q:
            # CreateMultipartUpload. DuckDB parses the UploadId out of this
            # response and then sends one UploadPart per part.
            upload_id = uuid.uuid4().hex
            upload_dir(bucket, key, upload_id)
            body = (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<InitiateMultipartUploadResult xmlns="{NS}">'
                f"<Bucket>{html.escape(bucket)}</Bucket>"
                f"<Key>{html.escape(key)}</Key>"
                f"<UploadId>{upload_id}</UploadId>"
                "</InitiateMultipartUploadResult>"
            ).encode()
            self._send_xml(200, body)
            return

        if "uploadId" in q and key:
            # CompleteMultipartUpload: concatenate the numbered parts in order
            # and publish with a rename so a reader never sees a partial file.
            upload_id = q["uploadId"][0]
            d = upload_dir(bucket, key, upload_id)
            parts = sorted(
                int(name[5:])
                for name in os.listdir(d)
                if name.startswith("part-") and name[5:].isdigit()
            )
            target = obj_path(bucket, key)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            tmp = target + ".multipart-tmp"
            with open(tmp, "wb") as out:
                for number in parts:
                    with open(os.path.join(d, f"part-{number}"), "rb") as part:
                        shutil.copyfileobj(part, out)
            os.replace(tmp, target)
            shutil.rmtree(d, ignore_errors=True)
            body = (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<CompleteMultipartUploadResult xmlns="{NS}">'
                f"<Location>http://localhost/{html.escape(bucket)}/{html.escape(key)}</Location>"
                f"<Bucket>{html.escape(bucket)}</Bucket>"
                f"<Key>{html.escape(key)}</Key>"
                '<ETag>&quot;s3mock-multipart&quot;</ETag>'
                "</CompleteMultipartUploadResult>"
            ).encode()
            self._send_xml(200, body)
            return

        self._send_empty(400)

    def do_PUT(self):
        bucket, key = self._bucket_key()
        q = self._query()
        data = self._drain_body()
        if not key:
            self._send_empty(400)
            return

        if "partNumber" in q and "uploadId" in q:
            number = int(q["partNumber"][0])
            d = upload_dir(bucket, key, q["uploadId"][0])
            with open(os.path.join(d, f"part-{number}"), "wb") as part:
                part.write(data)
            self.send_response(200)
            self.send_header("ETag", f'"part-{number}"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        p = obj_path(bucket, key)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p + ".multipart-tmp", "wb") as f:
            f.write(data)
        os.replace(p + ".multipart-tmp", p)
        self.send_response(200)
        self.send_header("ETag", '"mock"')
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):
        bucket, key = self._bucket_key()
        q = self._query()
        if "uploadId" in q:
            # AbortMultipartUpload.
            shutil.rmtree(upload_dir(bucket, key, q["uploadId"][0]), ignore_errors=True)
            self._send_empty(204)
            return
        try:
            os.remove(obj_path(bucket, key))
        except OSError:
            pass
        self._send_empty(204)

    def _read_range(self, p, size):
        rng = self.headers.get("Range")
        if rng:
            m = re.match(r"bytes=(\d+)-(\d*)", rng)
            if m:
                start = int(m.group(1))
                end = int(m.group(2)) if m.group(2) else size - 1
                end = min(end, size - 1)
                with open(p, "rb") as f:
                    f.seek(start)
                    data = f.read(end - start + 1)
                return data, start, end, 206
        with open(p, "rb") as f:
            data = f.read()
        return data, 0, size - 1, 200

    def do_HEAD(self):
        bucket, key = self._bucket_key()
        p = obj_path(bucket, key)
        if not os.path.isfile(p):
            self._send_empty(404)
            return
        size = os.path.getsize(p)
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header(
            "Last-Modified",
            datetime.now(timezone.utc).strftime("%a, %d %b %Y %H:%M:%S GMT"),
        )
        self.end_headers()

    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        bucket, key = self._bucket_key()
        q = urllib.parse.parse_qs(parsed.query)
        # ListObjectsV2
        if not key and "list-type" in q:
            prefix = q.get("prefix", [""])[0]
            base = os.path.join(ROOT, bucket)
            contents = []
            if os.path.isdir(base):
                for dirpath, _, files in os.walk(base):
                    for fn in files:
                        if fn.endswith(".multipart-tmp"):
                            continue
                        full = os.path.join(dirpath, fn)
                        rel = os.path.relpath(full, base).replace(os.sep, "/")
                        if rel.startswith(prefix):
                            contents.append((rel, os.path.getsize(full)))
            contents.sort()
            items = "".join(
                f"<Contents><Key>{html.escape(k)}</Key>"
                f"<LastModified>2026-01-01T00:00:00.000Z</LastModified>"
                f"<ETag>&quot;mock&quot;</ETag><Size>{s}</Size><StorageClass>STANDARD</StorageClass></Contents>"
                for k, s in contents
            )
            body = (
                '<?xml version="1.0" encoding="UTF-8"?>'
                f'<ListBucketResult xmlns="{NS}">'
                f"<Name>{html.escape(bucket)}</Name><Prefix>{html.escape(prefix)}</Prefix>"
                f"<KeyCount>{len(contents)}</KeyCount><MaxKeys>1000</MaxKeys>"
                "<IsTruncated>false</IsTruncated>" + items + "</ListBucketResult>"
            ).encode()
            self._send_xml(200, body)
            return
        # GetObject
        p = obj_path(bucket, key)
        if not os.path.isfile(p):
            self._send_empty(404)
            return
        size = os.path.getsize(p)
        data, start, end, status = self._read_range(p, size)
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
    print(f"s3mock listening on :{port}, root={ROOT}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
