# Narrow native-library binding only. CLI, config, HTTP, paths and output belong to Rust.
import json, sys, signal
signal.alarm(30)  # independent bound if the Rust parent is interrupted
import faiss
import numpy as np
request = json.load(sys.stdin)
index = faiss.read_index(sys.argv[1])
vector = np.array(request["vector"], dtype=np.float32).reshape(1, -1)
if index.d != vector.shape[1] or index.ntotal != request["count"]:
    raise ValueError("FAISS dimensions or metadata count mismatch")
scores, ids = index.search(vector, min(request["limit"], index.ntotal))
print(json.dumps([{"index": int(i), "similarity": float(s)}
                  for s, i in zip(scores[0], ids[0])], allow_nan=False))
