"""Deterministic pipeline fixture. This is NOT semantic-quality evidence."""
import json
import os
import sys
import time

request = json.load(sys.stdin)
if os.environ.get("MEMQ_TEST_EMBED_DELAY"):
    time.sleep(5)
vectors = []
for text in request["texts"]:
    # Deliberately arbitrary association, unrelated to natural-language meaning.
    vectors.append([1.0, 0.0, 0.0, 0.0] if ("quartz" in text or "cobalt" in text) else [0.0, 1.0, 0.0, 0.0])
json.dump(
    {
        "model": request["model"],
        "dimensions": 4,
        "preprocessing_version": request["preprocessing_version"],
        "vectors": vectors,
        "truncated": [False] * len(vectors),
    },
    sys.stdout,
)
