import json
path="/home/ubuntu/.myclaw/workspace/sessions/myclaw_s_019fe564-1566-7453-b9b0-89c5d707fa93/history.jsonl"
rows=[]
with open(path) as f:
    for i,line in enumerate(f):
        line=line.strip()
        if not line: continue
        try: m=json.loads(line)
        except: continue
        role=m.get('role')
        parts=m.get('parts',[])
        texts=[]
        for p in parts:
            if isinstance(p,dict):
                if p.get('type')=='text': texts.append(p.get('text',''))
                elif p.get('type')=='thinking': texts.append('[th]'+p.get('thinking','')[:40])
        rows.append((i,role,' '.join(texts)))
for i,role,t in rows[:28]:
    print(f"{i:4d} {role:9s} {t[:240].replace(chr(10),' ')}")
