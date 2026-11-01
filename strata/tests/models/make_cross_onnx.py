"""Writes the ONNX cross-encoders of this directory and
cross_onnx_reference.json, from the models make_cross.py wrote: ce_bert and
ce_xlmr exported to ONNX (ce_bert_onnx, ce_xlmr_onnx), ce_bert's export
quantized to int8 by ONNX Runtime's dynamic quantization, as Hugging Face
repositories ship their `onnx/model_qint8_*.onnx` (ce_bert_int8), and the
scores of make_cross.py's pairs by ONNX Runtime in Python, one pair at a
time, and by transformers with the pairs cut to 16 tokens.

    python3 -m venv /tmp/ce && . /tmp/ce/bin/activate
    pip install --index-url https://download.pytorch.org/whl/cpu torch==2.13.0
    pip install transformers==5.22.1 tokenizers safetensors onnx==1.23.2 onnxruntime==1.31.1
    python3 make_cross_onnx.py

tests/rank_cross.rs (feature onnx) checks Strata's cross_encoder() against
these scores and cross_reference.json's.
"""
import json
import os
import shutil

import numpy as np
import onnxruntime as ort
import torch
from onnxruntime.quantization import QuantType, quantize_dynamic
from transformers import AutoModelForSequenceClassification, PreTrainedTokenizerFast

HERE = os.path.dirname(os.path.abspath(__file__))
FILES = ["config.json", "tokenizer.json"]
SHORT = 16


def tokenizer(name):
    path = os.path.join(HERE, name, "tokenizer.json")
    if name == "ce_bert":
        return PreTrainedTokenizerFast(tokenizer_file=path, pad_token="[PAD]", cls_token="[CLS]", sep_token="[SEP]", unk_token="[UNK]"), True
    return PreTrainedTokenizerFast(tokenizer_file=path, bos_token="<s>", eos_token="</s>", pad_token="<pad>", unk_token="<unk>"), False


def export(name, out):
    model = AutoModelForSequenceClassification.from_pretrained(os.path.join(HERE, name)).eval()
    os.makedirs(out, exist_ok=True)
    for f in FILES:
        shutil.copy(os.path.join(HERE, name, f), os.path.join(out, f))
    tok, types = tokenizer(name)
    enc = tok("a query", "a text", return_token_type_ids=types, return_tensors="pt")
    names = ["input_ids", "attention_mask"] + (["token_type_ids"] if types else [])
    axes = {n: {0: "batch", 1: "tokens"} for n in names}
    axes["logits"] = {0: "batch"}
    torch.onnx.export(model, tuple(enc[n] for n in names), os.path.join(out, "model.onnx"), input_names=names, output_names=["logits"],
                      dynamic_axes=axes, opset_version=17, dynamo=False)
    return model


def onnx_scores(name, path, pairs, max_tokens):
    tok, types = tokenizer(name)
    s = ort.InferenceSession(os.path.join(path, "model.onnx"), providers=["CPUExecutionProvider"])
    out = []
    for q, t in pairs:
        enc = tok(q, t, truncation="longest_first", max_length=max_tokens, return_token_type_ids=types, return_tensors="np")
        feed = {i.name: enc[i.name].astype(np.int64) for i in s.get_inputs()}
        out.append(float(s.run(None, feed)[0][0, 0]))
    return out


def main():
    ref = json.load(open(os.path.join(HERE, "cross_reference.json")))
    pairs, max_tokens = [tuple(p) for p in ref["pairs"]], ref["max_tokens"]
    scores = {}
    for name in ["ce_bert", "ce_xlmr"]:
        out = os.path.join(HERE, f"{name}_onnx")
        model = export(name, out)
        scores[f"{name}_onnx"] = onnx_scores(name, out, pairs, max_tokens)
        # transformers' scores of the pairs cut to SHORT tokens.
        tok, types = tokenizer(name)
        with torch.no_grad():
            scores[f"{name}@{SHORT}"] = [float(model(**tok(q, t, truncation="longest_first", max_length=SHORT, return_token_type_ids=types, return_tensors="pt")).logits[0, 0]) for q, t in pairs]
    out = os.path.join(HERE, "ce_bert_int8")
    os.makedirs(out, exist_ok=True)
    for f in FILES:
        shutil.copy(os.path.join(HERE, "ce_bert", f), os.path.join(out, f))
    quantize_dynamic(os.path.join(HERE, "ce_bert_onnx", "model.onnx"), os.path.join(out, "model.onnx"), weight_type=QuantType.QInt8)
    scores["ce_bert_int8"] = onnx_scores("ce_bert", out, pairs, max_tokens)
    with open(os.path.join(HERE, "cross_onnx_reference.json"), "w") as f:
        json.dump({"max_tokens": max_tokens, "short": SHORT, "onnxruntime": ort.__version__, "scores": scores}, f, indent=1)
        f.write("\n")
    for name, s in scores.items():
        print(name, " ".join(f"{x:.4f}" for x in s))
    for name in ["ce_bert", "ce_xlmr"]:
        d = max(abs(a - b) for a, b in zip(scores[f"{name}_onnx"], ref["scores"][name]))
        print(f"{name}: ONNX Runtime against transformers, largest difference {d:.2e}")


if __name__ == "__main__":
    main()
