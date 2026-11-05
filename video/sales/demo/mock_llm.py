import json, http.server
TXT = "Refund $42.00. Damaged on arrival, inside the 30-day window."
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); body=json.loads(self.rfile.read(n))
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.end_headers()
        def ev(o): self.wfile.write(b"data: "+json.dumps(o).encode()+b"\n\n"); self.wfile.flush()
        base={"id":"x","object":"chat.completion.chunk","created":0,"model":body.get('model')}
        for w in TXT.split(" "):
            ev({**base,"choices":[{"index":0,"delta":{"content":w+" "},"finish_reason":None}]})
        ev({**base,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ev({**base,"choices":[],"usage":{"prompt_tokens":1840,"completion_tokens":96,"total_tokens":1936}})
        self.wfile.write(b"data: [DONE]\n\n")
    def log_message(self,*a): pass
http.server.HTTPServer(('127.0.0.1',8977),H).serve_forever()
