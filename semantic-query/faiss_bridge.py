"""Native-library binding only; CLI, config, HTTP, validation and formatting live in Rust."""
import json, sys, signal
signal.alarm(30)
try:
    import faiss
    import numpy as np
except ImportError:
    sys.exit("FAISS runtime import failed; install the pinned runtime requirements")
request = json.load(sys.stdin)
try:
    index = faiss.read_index(sys.argv[1])
except Exception:
    sys.exit("FAISS index unreadable; existing index was not changed")
vector = np.array(request["vector"], dtype=np.float32).reshape(1, -1)
if index.d != vector.shape[1]:
    sys.exit("embedding dimension disagrees with FAISS index")
if index.ntotal != request["count"]:
    sys.exit("FAISS vector count disagrees with metadata count; index drift")
if index.metric_type != faiss.METRIC_INNER_PRODUCT:
    sys.exit("unsupported index metric; expected cosine inner product")
scores, ids = index.search(vector, min(request["limit"], index.ntotal))
print(json.dumps([{"index": int(i), "similarity": float(s)}
                  for i, s in zip(ids[0], scores[0]) if i >= 0], allow_nan=False))
