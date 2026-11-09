"""Fake DIRIGERA hub for testing dirigera-tui without hardware.

Usage (from the repo root):
    openssl req -x509 -newkey rsa:2048 -nodes -keyout tools/key.pem \
        -out tools/cert.pem -days 30 -subj /CN=hub
    python3 tools/mock_hub.py
    DIRIGERA_HOST=127.0.0.1:18443 DIRIGERA_TOKEN=TOK cargo run

Logs every PATCH body to stderr. Device data is invented, not captured from a real hub (except the `deviceSet`
shape, which matches a real hub).
"""
import os
import json, ssl, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
R={"id":"r1","name":"Living room"}; K={"id":"r2","name":"Kitchen"}
devs=[
 {"id":"a","deviceType":"light","isReachable":True,"room":R,"attributes":{"customName":"Ceiling","model":"TRADFRI bulb","firmwareVersion":"1.0","isOn":True,"lightLevel":70,"colorTemperature":2700,"colorTemperatureMin":4000,"colorTemperatureMax":2202,"colorHue":30,"colorSaturation":0.5}},
 {"id":"b","deviceType":"light","isReachable":True,"room":R,"attributes":{"customName":"Floor lamp","isOn":False,"lightLevel":40}},
 {"id":"s1","deviceType":"light","isReachable":True,"room":K,"deviceSet":[{"id":"set1","name":"Spots","icon":"lighting_group"}],"attributes":{"customName":"Spot 1","isOn":True,"lightLevel":50,"colorTemperature":3000,"colorTemperatureMin":4000,"colorTemperatureMax":2202}},
 {"id":"s2","deviceType":"light","isReachable":True,"room":K,"deviceSet":[{"id":"set1","name":"Spots","icon":"lighting_group"}],"attributes":{"customName":"Spot 2","isOn":False,"lightLevel":80,"colorTemperature":3000,"colorTemperatureMin":4000,"colorTemperatureMax":2202}},
 {"id":"c","deviceType":"outlet","isReachable":True,"room":K,"attributes":{"customName":"Coffee","isOn":True}},
 {"id":"d","deviceType":"environmentSensor","isReachable":True,"room":K,"attributes":{"customName":"VINDSTYRKA","currentTemperature":21.4,"currentRH":44,"currentPM25":3}},
 {"id":"e","deviceType":"blinds","isReachable":False,"room":R,"attributes":{"customName":"Blind","blindsCurrentLevel":20,"blindsTargetLevel":20,"batteryPercentage":80}},
 {"id":"g","deviceType":"gateway","isReachable":True,"attributes":{"customName":"hub"}},
]
scenes=[{"id":"s1","info":{"name":"Movie night"}},{"id":"s2","info":{"name":"Good morning"}}]
class H(BaseHTTPRequestHandler):
    def log_message(self,*a): sys.stderr.write(self.command+" "+self.path+"\n")
    def send(self,obj,code=200):
        b=json.dumps(obj).encode(); self.send_response(code); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.headers.get("Authorization")!="Bearer TOK": return self.send({},401)
        self.send(devs if self.path=="/v1/devices" else scenes)
    def do_PATCH(self):
        body=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        sys.stderr.write("BODY "+json.dumps(body)+"\n")
        i=self.path.split("/")[-1]
        for d in devs:
            if d["id"]==i: d["attributes"].update(body[0]["attributes"])
        self.send_response(202); self.send_header("Content-Length","0"); self.end_headers()
    def do_POST(self):
        self.send_response(202); self.send_header("Content-Length","0"); self.end_headers()
s=HTTPServer(("127.0.0.1",18443),H); c=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); d=os.path.dirname(os.path.abspath(__file__)); c.load_cert_chain(os.path.join(d,"cert.pem"),os.path.join(d,"key.pem")); s.socket=c.wrap_socket(s.socket,server_side=True); s.serve_forever()
