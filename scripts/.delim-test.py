import sys, os, urllib.parse
sys.path.insert(0, os.path.abspath("scripts"))
from s3probe import S3Client
ep, bucket, ak, sk, ca = sys.argv[1:6]
c = S3Client(ep, ak, sk, bucket=bucket, scheme="https", ca_bundle=ca)
c.request("PUT", path=f"/{bucket}/probe/0001/a.ltx", body=b"hello")
c.request("PUT", path=f"/{bucket}/probe/0001/b.ltx", body=b"hello")
st, h, body = c.request("GET", query=[("list-type", "2"), ("prefix", "probe/")])
print("plain list status", st, body[:200].decode())
try:
    st, h, body = c.request("GET", query=[("list-type", "2"), ("prefix", "probe/"), ("delimiter", "/")])
    print("delimiter list status", st, body[:300].decode())
except Exception as e:
    print("delimiter list ERROR:", type(e).__name__, e)
