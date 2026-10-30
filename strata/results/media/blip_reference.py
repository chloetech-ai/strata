# Captions of the first n images of a directory (by file name) by BLIP base
# through transformers, as JSON of file name to caption, to compare with
# the captions strata-media-eval writes (--captions-out).
import sys, os, time, json
import torch
from PIL import Image
from transformers import BlipProcessor, BlipForConditionalGeneration
torch.set_num_threads(4)
model_dir, img_dir, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
proc = BlipProcessor.from_pretrained(model_dir)
model = BlipForConditionalGeneration.from_pretrained(model_dir).eval()
files = sorted(os.listdir(img_dir))[:n]
out = {}
t = time.time()
with torch.no_grad():
    for f in files:
        img = Image.open(os.path.join(img_dir, f)).convert("RGB")
        inputs = proc(images=img, return_tensors="pt")
        ids = model.generate(**inputs)
        out[f] = proc.decode(ids[0], skip_special_tokens=True).strip()
print(json.dumps(out, indent=1))
print("secs/image", (time.time() - t) / len(files), file=sys.stderr)
