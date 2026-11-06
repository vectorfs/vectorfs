#!/usr/bin/env python3
"""Regenerate the tiny-reranker ONNX fixture (M3-T11, the reranker axis's
hermetic I/O-contract twin of tiny-embedder).

The committed artifacts (model.onnx / tokenizer.json / config.json) were
produced by this script; rerun it only when the fixture shape must change
(every test that pins scoring VALUES will need updating too).

Design constraints the fixture honors:
  * graph mirrors a transformers-style sequence-classification reranker
    export: inputs `input_ids` + `attention_mask` + `token_type_ids`
    (all [batch, seq] int64, dynamic dims — the real
    gte-multilingual-reranker-base fp32 export declares exactly these
    three), ONE output `logits` [batch, 1] float32 — the cross-encoder
    kind's startup probe requires exactly that one-logit-per-pair shape;
  * the computation is a fixed arithmetic pattern (no RNG, committed bytes
    reproducible): logit[row] = sum over masked positions of
    (token_id + 1) — a Gather-free Cast/Add/Mul/ReduceSum chain, so longer
    or higher-id token pairs score higher and e2e ORDER assertions have a
    deterministic oracle;
  * tokenizer.json is a minimal WordLevel file (16-token vocab, the
    tiny-embedder vocabulary) with a PAIR template post-processor
    (`[CLS] A [SEP] B [SEP]`) — the cross-encoder encodes (query, doc)
    DUAL inputs and the pair template does the joining, the same
    tokenizers-crate code path the real gte reranker tokenizer.json takes.

Usage: python3 generate.py <output-dir>   (default: alongside this script)
"""

import json
import pathlib
import sys

import onnx
from onnx import TensorProto, helper

VOCAB = {
    "[PAD]": 0,
    "[UNK]": 1,
    "[CLS]": 2,
    "[SEP]": 3,
    "contract": 4,
    "probe": 5,
    "first": 6,
    "chunk": 7,
    "second": 8,
    "hello": 9,
    "vectorfs": 10,
    "the": 11,
    "quick": 12,
    "brown": 13,
    "fox": 14,
    "embeds": 15,
}


def build_model() -> onnx.ModelProto:
    ids = helper.make_tensor_value_info(
        "input_ids", TensorProto.INT64, ["batch", "seq"]
    )
    mask = helper.make_tensor_value_info(
        "attention_mask", TensorProto.INT64, ["batch", "seq"]
    )
    tt = helper.make_tensor_value_info(
        "token_type_ids", TensorProto.INT64, ["batch", "seq"]
    )
    out = helper.make_tensor_value_info(
        "logits", TensorProto.FLOAT, ["batch", 1]
    )

    # logit[row] = ReduceSum_seq( (Cast(input_ids) + 1) * Cast(mask) )
    # — masked pad positions contribute 0; every real token contributes
    # id+1. Deterministic, order-free per row, monotone in content length.
    cast_ids = helper.make_node("Cast", ["input_ids"], ["ids_f"], to=TensorProto.FLOAT)
    add = helper.make_node("Add", ["ids_f", "one"], ["ids_plus"])
    cast_mask = helper.make_node("Cast", ["attention_mask"], ["mask_f"], to=TensorProto.FLOAT)
    mul = helper.make_node("Mul", ["ids_plus", "mask_f"], ["weighted"])
    # opset 13+: ReduceSum takes axes as an INPUT tensor, not an attribute
    red = helper.make_node("ReduceSum", ["weighted", "seq_axis"], ["logits"], keepdims=1)

    import numpy as np
    one_init = onnx.numpy_helper.from_array(np.array(1.0, dtype="float32"), name="one")
    axis_init = onnx.numpy_helper.from_array(np.array([1], dtype="int64"), name="seq_axis")

    graph = helper.make_graph(
        [cast_ids, add, cast_mask, mul, red],
        "tiny-reranker",
        [ids, mask, tt],
        [out],
        initializer=[one_init, axis_init],
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="vectorfs fixture generator",
    )
    # IR 9 = comfortably inside ONNX Runtime 1.31's accepted range (the
    # tiny-embedder fixture's pin; keep the two fixtures aligned).
    model.ir_version = 9
    onnx.checker.check_model(model)
    return model


def build_tokenizer() -> dict:
    return {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [
            {"id": i, "content": tok, "single_word": False, "lstrip": False,
             "rstrip": False, "normalized": False, "special": True}
            for tok, i in VOCAB.items() if tok.startswith("[") and tok.endswith("]")
        ],
        "normalizer": {"type": "Lowercase"},
        "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [
                {"SpecialToken": {"id": "[CLS]", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 0}},
            ],
            # THE pair template (the cross-encoder's (query, doc) dual
            # input): [CLS] query [SEP] doc [SEP]
            "pair": [
                {"SpecialToken": {"id": "[CLS]", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 0}},
                {"Sequence": {"id": "B", "type_id": 1}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 1}},
            ],
            "special_tokens": {
                "[CLS]": {"id": "[CLS]", "ids": [2], "tokens": ["[CLS]"]},
                "[SEP]": {"id": "[SEP]", "ids": [3], "tokens": ["[SEP]"]},
            },
        },
        "decoder": None,
        "model": {
            "type": "WordLevel",
            "vocab": VOCAB,
            "unk_token": "[UNK]",
        },
    }


def main() -> None:
    out_dir = pathlib.Path(
        sys.argv[1] if len(sys.argv) > 1 else pathlib.Path(__file__).parent
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    onnx.save(build_model(), out_dir / "model.onnx")

    with open(out_dir / "tokenizer.json", "w", encoding="utf-8") as f:
        json.dump(build_tokenizer(), f, indent=2, ensure_ascii=False)
        f.write("\n")

    with open(out_dir / "config.json", "w", encoding="utf-8") as f:
        json.dump(
            {
                "model_type": "tiny-fixture",
                "license": "CC0-1.0 (vectorfs test fixture, generated in-repo)",
                "task": "text-classification (cross-encoder pair scoring)",
            },
            f,
            indent=2,
        )
        f.write("\n")

    print(f"wrote {out_dir}/model.onnx tokenizer.json config.json")


if __name__ == "__main__":
    main()
