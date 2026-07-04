#!/usr/bin/env python3
"""Esporta un grafo stradale di Torino (centro) in JSON per il simulatore WASM.

Stesso scopo di OSMnx (grafo guidabile da OpenStreetMap), ma senza le sue
dipendenze pesanti: usa solo la stdlib e l'API Overpass via HTTP.

Output: web/data/graph.json
    {
      "nodes": [[lat, lon], ...],     # indice = id nodo
      "adj":   [[j, k, ...], ...]     # adj[i] = nodi vicini (grafo non orientato)
    }

I nodi sono i punti OSM delle strade guidabili; gli archi collegano nodi
consecutivi lungo ogni way. Il simulatore cammina di nodo in nodo seguendo la
forma reale delle strade e sceglie la direzione agli incroci.

Uso:
    python tools/export_turin_graph.py
"""
import json
import os
import urllib.request

# Bounding box centro Torino (south, west, north, east). Piccolo apposta:
# tiene il file leggero (spec: attenzione alla dimensione) e il giro nel centro.
SOUTH, WEST, NORTH, EAST = 45.050, 7.660, 45.090, 7.705

# Tipi di strada percorribili in auto.
HIGHWAY = (
    "motorway|trunk|primary|secondary|tertiary|unclassified|residential|"
    "living_street|motorway_link|trunk_link|primary_link|secondary_link|"
    "tertiary_link"
)

OVERPASS_URL = "https://overpass-api.de/api/interpreter"

QUERY = f"""
[out:json][timeout:90];
way["highway"~"^({HIGHWAY})$"]({SOUTH},{WEST},{NORTH},{EAST});
(._;>;);
out body;
"""

OUT_PATH = os.path.join(os.path.dirname(__file__), "..", "web", "data", "graph.json")


def fetch():
    print("Query Overpass (Torino centro)...")
    data = urllib.parse.urlencode({"data": QUERY}).encode()
    # Overpass rifiuta lo User-Agent di default di urllib con 406: dichiarane uno.
    req = urllib.request.Request(
        OVERPASS_URL,
        data=data,
        headers={"User-Agent": "georuggine-exporter/1.0", "Accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.load(resp)


def build_graph(osm):
    coords = {}          # osm_id -> (lat, lon)
    ways = []            # lista di liste di osm_id
    for el in osm["elements"]:
        if el["type"] == "node":
            coords[el["id"]] = (el["lat"], el["lon"])
        elif el["type"] == "way" and "nodes" in el:
            ways.append(el["nodes"])

    # Tieni solo i nodi effettivamente usati dalle way e dai loro un indice 0..N.
    used = []
    seen = set()
    for w in ways:
        for nid in w:
            if nid in coords and nid not in seen:
                seen.add(nid)
                used.append(nid)
    index = {nid: i for i, nid in enumerate(used)}
    nodes = [[coords[nid][0], coords[nid][1]] for nid in used]

    # Adiacenze da coppie consecutive (grafo non orientato; per la demo
    # ignoriamo i sensi unici).
    adj = [set() for _ in nodes]
    for w in ways:
        prev = None
        for nid in w:
            if nid not in index:
                prev = None
                continue
            cur = index[nid]
            if prev is not None and prev != cur:
                adj[prev].add(cur)
                adj[cur].add(prev)
            prev = cur

    return {"nodes": nodes, "adj": [sorted(s) for s in adj]}


def main():
    osm = fetch()
    graph = build_graph(osm)
    os.makedirs(os.path.dirname(OUT_PATH), exist_ok=True)
    with open(OUT_PATH, "w", encoding="utf-8") as f:
        json.dump(graph, f, separators=(",", ":"))
    size_kb = os.path.getsize(OUT_PATH) / 1024
    n_edges = sum(len(a) for a in graph["adj"]) // 2
    print(f"OK: {len(graph['nodes'])} nodi, {n_edges} archi -> {OUT_PATH} ({size_kb:.0f} KB)")


if __name__ == "__main__":
    main()
