"""CLDR ru annotations (+ derived) -> TSV: эмодзи \t имя \t ключ|ключ. Запуск: python -I cldr_ru.py full.json [derived.json] > emoji-ru.tsv"""
import json, sys

rows = {}
for path in sys.argv[1:]:
    d = json.load(open(path, encoding="utf-8"))
    ann = d.get("annotations") or d.get("annotationsDerived")
    for e, v in ann["annotations"].items():
        name = (v.get("tts") or [""])[0].strip()
        kw = [k.strip() for k in v.get("default", []) if k.strip() and k.strip() != name]
        if any(0x1F3FB <= ord(c) <= 0x1F3FF for c in e):
            continue  # только жёлтые (оттенки кожи не показываем)
        if name and e not in rows:
            rows[e] = (name, kw)
out = sys.stdout.buffer
for e in sorted(rows):
    name, kw = rows[e]
    out.write(f"{e}\t{name}\t{'|'.join(kw)}\n".replace("\r", "").encode("utf-8"))
