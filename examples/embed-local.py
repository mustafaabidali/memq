#!/usr/bin/env python3
"""Use a local E5 model with memq.

Install sentence-transformers to run the model. Choose a model explicitly.
With no options, read one memq JSON request from stdin and print its vectors.
Use --serve PORT --model NAME to keep the model loaded between requests.
Use --url http://127.0.0.1:PORT to send requests to that local server.
The --url mode needs only Python's standard library.
"""
import argparse
import contextlib
import http.server
import json
import sys
import urllib.request


def load_model(identity):
    from sentence_transformers import SentenceTransformer

    name, separator, revision = identity.partition("@")
    with contextlib.redirect_stdout(sys.stderr):
        model = SentenceTransformer(
            name,
            revision=revision if separator else None,
            device="cpu",
            trust_remote_code=False,
        )
    return model


def encode(model, identity, request):
    if request["model"] != identity:
        raise ValueError("model mismatch")
    if request["preprocessing_version"] != "e5-prefix-v1":
        raise ValueError("unsupported preprocessing")
    if request["kind"] not in ("passage", "query"):
        raise ValueError("unsupported input kind")
    texts = [request["kind"] + ": " + text for text in request["texts"]]
    lengths = [
        len(model.tokenizer(text, truncation=False)["input_ids"]) for text in texts
    ]
    with contextlib.redirect_stdout(sys.stderr):
        vectors = model.encode(
            texts, normalize_embeddings=True, show_progress_bar=False
        ).tolist()
    return {
        "model": identity,
        "dimensions": model.get_sentence_embedding_dimension(),
        "preprocessing_version": "e5-prefix-v1",
        "vectors": vectors,
        "truncated": [n > model.max_seq_length for n in lengths],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", help="Send stdin to this local model server.")
    parser.add_argument("--serve", type=int, help="Listen on this loopback port.")
    parser.add_argument("--model", help="Model name, with optional @revision.")
    args = parser.parse_args()
    if args.url:
        if not args.url.startswith(("http://127.0.0.1:", "http://localhost:")):
            raise ValueError("this example only forwards to a local server")
        request = urllib.request.Request(
            args.url,
            data=sys.stdin.buffer.read(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=60) as response:
            sys.stdout.buffer.write(response.read())
        return
    if args.serve is not None:
        if not args.model:
            parser.error("--serve requires --model")
        model = load_model(args.model)

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if size > 64 * 1024 * 1024:
                        raise ValueError("request too large")
                    request = json.loads(self.rfile.read(size))
                    data = json.dumps(
                        encode(model, args.model, request), separators=(",", ":")
                    ).encode()
                    self.send_response(200)
                except Exception:
                    data = b'{"error":"embedding request failed"}'
                    self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        server = http.server.HTTPServer(("127.0.0.1", args.serve), Handler)
        print(json.dumps({"ready": True, "port": server.server_port}), flush=True)
        server.serve_forever()
    else:
        request = json.load(sys.stdin)
        identity = request["model"]
        json.dump(
            encode(load_model(identity), identity, request),
            sys.stdout,
            separators=(",", ":"),
        )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Do not echo requests, provider responses, or local source paths.
        print(type(error).__name__, file=sys.stderr)
        sys.exit(1)
