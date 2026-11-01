"""Writes colbert_small, a ColBERT checkpoint as the colbert-ai library
saves one (config.json, model.safetensors with a BERT under `bert.` and the
projection `linear.weight`, tokenizer.json, artifact.metadata), with random
weights and a tokenizer trained here, and colbert_reference.json: the
vectors of a few documents' and queries' tokens computed in PyTorch the way
colbert-ai computes them (its DocTokenizer and QueryTokenizer: ". " before
the text, the "." replaced by the marker, queries padded with [MASK] to
query_maxlen, documents' punctuation left out, every vector projected and
scaled to unit length).

    python3 -m venv /tmp/ce && . /tmp/ce/bin/activate
    pip install --index-url https://download.pytorch.org/whl/cpu torch==2.13.0
    pip install transformers==5.22.1 tokenizers safetensors
    python3 make_colbert.py

tests/embed_tokens.rs (feature bert) checks Strata's ColBERT vectors against
colbert_reference.json.
"""
import json
import os
import string

import torch
from safetensors.torch import save_file
from tokenizers import BertWordPieceTokenizer
from tokenizers.processors import BertProcessing
from transformers import BertConfig, BertModel, PreTrainedTokenizerFast

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "colbert_small")
torch.manual_seed(20261101)

CORPUS = [
    "Late interaction keeps a vector for every token of a passage.",
    "A query's tokens each find their best match among the passage's tokens, and the matches add up.",
    "ColBERT scores passages by the sum of maximum similarities, MaxSim.",
    "Punctuation, such as commas and full stops, gets no vector of its own!",
    "Strata ranks the top rows of BM25 again with maxsim over stored token vectors.",
    "Short queries are padded with masks: query augmentation.",
]
DOCS = CORPUS + [
    "",
    "Late, interaction!",
    "A query (or a passage): tokens.",
    "One very long passage that goes on and on, well past the number of tokens a document may have, so that it is cut short before its end, as the library cuts it.",
]
QUERIES = ["late interaction", "how are passages scored?", "a query long enough to be cut at its maximum length of tokens", "masks"]
META = {
    "query_token_id": "[unused0]",
    "doc_token_id": "[unused1]",
    "query_token": "[Q]",
    "doc_token": "[D]",
    "query_maxlen": 8,
    "doc_maxlen": 24,
    "dim": 16,
    "mask_punctuation": True,
    "attend_to_mask_tokens": False,
    "similarity": "cosine",
}


def main():
    os.makedirs(OUT, exist_ok=True)
    wp = BertWordPieceTokenizer(lowercase=True)
    wp.train_from_iterator(CORPUS, vocab_size=220, special_tokens=["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]", "[unused0]", "[unused1]"])
    # A tokenizer trained from nothing has no post-processor: [CLS] text [SEP], as BERT's.
    wp.post_processor = BertProcessing(("[SEP]", wp.token_to_id("[SEP]")), ("[CLS]", wp.token_to_id("[CLS]")))
    wp.save(os.path.join(OUT, "tokenizer.json"))
    tok = PreTrainedTokenizerFast(tokenizer_file=os.path.join(OUT, "tokenizer.json"), pad_token="[PAD]", cls_token="[CLS]", sep_token="[SEP]", unk_token="[UNK]", mask_token="[MASK]")
    cfg = BertConfig(vocab_size=len(tok), hidden_size=32, num_hidden_layers=2, num_attention_heads=4, intermediate_size=64, max_position_embeddings=64, initializer_range=0.2)
    bert = BertModel(cfg, add_pooling_layer=False).eval()
    linear = torch.nn.Linear(32, META["dim"], bias=False)
    torch.nn.init.normal_(linear.weight, std=0.3)
    cfg.architectures = ["HF_ColBERT"]
    cfg.save_pretrained(OUT)
    weights = {f"bert.{k}": v.contiguous() for k, v in bert.state_dict().items()}
    weights["linear.weight"] = linear.weight.detach().contiguous()
    save_file(weights, os.path.join(OUT, "model.safetensors"), metadata={"format": "pt"})
    with open(os.path.join(OUT, "artifact.metadata"), "w") as f:
        json.dump(META, f, indent=2)

    q_marker = tok.convert_tokens_to_ids("[unused0]")
    d_marker = tok.convert_tokens_to_ids("[unused1]")
    skip = {tok.encode(c, add_special_tokens=False)[0] for c in string.punctuation if tok.encode(c, add_special_tokens=False)}

    def vectors(ids, mask):
        with torch.no_grad():
            out = linear(bert(input_ids=ids, attention_mask=mask).last_hidden_state)
        return torch.nn.functional.normalize(out, p=2, dim=2)

    # colbert-ai's DocTokenizer.tensorize and ColBERT.doc.
    enc = tok([". " + d for d in DOCS], padding="longest", truncation="longest_first", max_length=META["doc_maxlen"], return_tensors="pt")
    ids, mask = enc["input_ids"], enc["attention_mask"]
    ids[:, 1] = d_marker
    D = vectors(ids, mask)
    docs = []
    for i, text in enumerate(DOCS):
        keep = [j for j, x in enumerate(ids[i].tolist()) if x not in skip and x != tok.pad_token_id]
        docs.append({"text": text, "vectors": [[round(x, 6) for x in D[i, j].tolist()] for j in keep]})

    # colbert-ai's QueryTokenizer.tensorize and ColBERT.query.
    enc = tok([". " + q for q in QUERIES], padding="max_length", truncation=True, max_length=META["query_maxlen"], return_tensors="pt")
    ids, mask = enc["input_ids"], enc["attention_mask"]
    ids[:, 1] = q_marker
    ids[ids == tok.pad_token_id] = tok.mask_token_id
    Q = vectors(ids, mask)
    queries = [{"text": q, "vectors": [[round(x, 6) for x in v] for v in Q[i].tolist()]} for i, q in enumerate(QUERIES)]
    with open(os.path.join(HERE, "colbert_reference.json"), "w") as f:
        json.dump({"model": "colbert_small", "dim": META["dim"], "docs": docs, "queries": queries}, f)
    print(f"{len(docs)} documents, {len(queries)} queries, vocabulary {len(tok)}")


if __name__ == "__main__":
    main()
