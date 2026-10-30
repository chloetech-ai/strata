"""Writes the cross-encoders of this directory and cross_reference.json:
two small models with random weights, a BERT one (ce_bert, a
BertForSequenceClassification) and an XLM-RoBERTa one (ce_xlmr), each
with a tokenizer trained here, and their scores of query and text pairs
by Hugging Face transformers in PyTorch, one pair at a time.

    python3 -m venv /tmp/ce && . /tmp/ce/bin/activate
    pip install --index-url https://download.pytorch.org/whl/cpu torch==2.15.0
    pip install transformers==5.22.1 tokenizers safetensors
    python3 make_cross.py

Each model reads at most 64 tokens, so the long pairs are cut, the longer
of query and text first. tests/rank_cross.rs (feature bert) checks
Strata's cross_encoder() against cross_reference.json.
"""
import json
import os

import torch
from tokenizers import BertWordPieceTokenizer, SentencePieceUnigramTokenizer
from tokenizers.processors import BertProcessing, RobertaProcessing
from transformers import (BertConfig, BertForSequenceClassification, PreTrainedTokenizerFast,
                          XLMRobertaConfig, XLMRobertaForSequenceClassification)

HERE = os.path.dirname(os.path.abspath(__file__))
torch.manual_seed(20261030)

CORPUS = [
    "Graph neural networks learn representations of nodes from their neighbours.",
    "A cross-encoder reads the query and the passage together and scores them.",
    "BM25 ranks documents by the frequency of query terms, saturated by length.",
    "The capital of Chile is Santiago, in a valley between two mountain ranges.",
    "Vaccines train the immune system to recognise a virus before an infection.",
    "Coffee contains caffeine, which blocks adenosine receptors in the brain.",
    "Inverted indexes map each term to the documents that contain it.",
    "Transformers use attention to mix information across every token of a text.",
    "Glaciers in Patagonia have retreated quickly over the last century.",
    "A learning to rank model orders results by features such as text scores.",
    "Rust has no garbage collector; ownership decides when memory is freed.",
    "The Atacama desert is one of the driest places on Earth.",
    "Search engines rerank the top results of a cheap first stage with costly models.",
    "Protein folding predicts the shape of a protein from its amino acid sequence.",
    "Tokenizers split text into word pieces that a model has embeddings for.",
    "Ocean currents carry heat from the equator towards the poles.",
]

PAIRS = [
    ("graph neural networks", CORPUS[0]),
    ("graph neural networks", CORPUS[3]),
    ("what does a cross-encoder do", CORPUS[1]),
    ("bm25 ranking", CORPUS[2]),
    ("capital of chile", CORPUS[3]),
    ("capital of chile", CORPUS[11]),
    ("how do vaccines work", CORPUS[4]),
    ("caffeine and the brain", CORPUS[5]),
    ("inverted index", CORPUS[6] + " " + CORPUS[2]),
    ("rerank", CORPUS[12]),
    ("", CORPUS[7]),
    ("glaciers", ""),
    # Longer than a model reads: cut, the longer side first.
    ("long passage about search and ranking", " ".join(CORPUS)),
    (" ".join(CORPUS[:6]), "a short text"),
    (" ".join(CORPUS[:5]), " ".join(CORPUS[5:])),
    ("UNSEEN wörds ünd Ñandú 42", "Ñandú: a bird of Patagonia, 1,5 metres tall!"),
]

MAX_TOKENS = 64


def reference(model, tokenizer, types):
    model.eval()
    out = []
    with torch.no_grad():
        for q, t in PAIRS:
            enc = tokenizer(q, t, truncation="longest_first", max_length=MAX_TOKENS, return_token_type_ids=types, return_tensors="pt")
            out.append(float(model(**enc).logits[0, 0]))
    return out


def bert():
    tok = BertWordPieceTokenizer(lowercase=True)
    tok.train_from_iterator(CORPUS, vocab_size=400, special_tokens=["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"])
    tok.post_processor = BertProcessing(("[SEP]", 3), ("[CLS]", 2))
    config = BertConfig(vocab_size=tok.get_vocab_size(), hidden_size=32, num_hidden_layers=2, num_attention_heads=4, intermediate_size=64,
                        max_position_embeddings=MAX_TOKENS, type_vocab_size=2, pad_token_id=0, num_labels=1, initializer_range=0.2)
    model = BertForSequenceClassification(config)
    out = os.path.join(HERE, "ce_bert")
    model.save_pretrained(out, safe_serialization=True)
    tok.save(os.path.join(out, "tokenizer.json"))
    fast = PreTrainedTokenizerFast(tokenizer_file=os.path.join(out, "tokenizer.json"), pad_token="[PAD]", cls_token="[CLS]", sep_token="[SEP]", unk_token="[UNK]")
    # The text is the second segment, as BERT's tokenizers mark it.
    return reference(model, fast, True)


def xlmr():
    specials = ["<s>", "<pad>", "</s>", "<unk>"]
    tok = SentencePieceUnigramTokenizer()
    tok.train_from_iterator(CORPUS, vocab_size=300, special_tokens=specials, unk_token="<unk>")
    tok.post_processor = RobertaProcessing(("</s>", 2), ("<s>", 0))
    config = XLMRobertaConfig(vocab_size=tok.get_vocab_size(), hidden_size=32, num_hidden_layers=2, num_attention_heads=4, intermediate_size=64,
                              max_position_embeddings=MAX_TOKENS + 2, type_vocab_size=1, pad_token_id=1, bos_token_id=0, eos_token_id=2,
                              num_labels=1, initializer_range=0.2)
    model = XLMRobertaForSequenceClassification(config)
    out = os.path.join(HERE, "ce_xlmr")
    model.save_pretrained(out, safe_serialization=True)
    tok.save(os.path.join(out, "tokenizer.json"))
    fast = PreTrainedTokenizerFast(tokenizer_file=os.path.join(out, "tokenizer.json"), bos_token="<s>", eos_token="</s>", pad_token="<pad>", unk_token="<unk>")
    return reference(model, fast, False)


def main():
    scores = {"ce_bert": bert(), "ce_xlmr": xlmr()}
    with open(os.path.join(HERE, "cross_reference.json"), "w") as f:
        json.dump({"max_tokens": MAX_TOKENS, "pairs": [list(p) for p in PAIRS], "scores": scores}, f, indent=1)
        f.write("\n")
    for name, s in scores.items():
        print(name, " ".join(f"{x:.4f}" for x in s))


if __name__ == "__main__":
    main()
