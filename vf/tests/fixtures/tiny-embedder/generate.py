#!/usr/bin/env python3
"""Regenerate the tiny-embedder ONNX fixture (M2-T5, design.md SS5.10.7
"CI 用仓内 ~1MB 小 ONNX fixture 保 hermetic").

The committed artifacts (model.onnx / tokenizer.json / config.json) were
produced by this script; rerun it only when the fixture shape must change
(every test that pins embedding VALUES will need updating too).

Design constraints the fixture honors:
  * graph mirrors a transformers-style embedder export: inputs
    `input_ids` + `attention_mask` (both [batch, seq] int64, dynamic dims),
    ONE output `last_hidden_state` [batch, seq, 8] with a STATIC last
    dimension -- the ort kind's startup dim probe reads exactly that;
  * a single Gather over a constant table: tiny, deterministic, zero
    randomness (values are a fixed arithmetic pattern, not RNG, so the
    committed bytes are reproducible bit-for-bit);
  * tokenizer.json is a minimal WordLevel file (16-token vocab) with
    [CLS]/[SEP] template post-processing -- the same tokenizers-crate code
    path the real gte tokenizer.json takes, minus the size.

Usage: python3 generate.py <output-dir>   (default: alongside this script)
"""

import json
import pathlib
import sys

import onnx
from onnx import TensorProto, helper, numpy_helper

import numpy as np

VOCAB_SIZE = 16
DIM = 8

# word-level vocab: specials first (ids 0-3 mirror the BERT convention the
# runtime code relies on when it reads attention masks), then plain words
# covering the test corpus ("contract probe", "first chunk", "second chunk",
# CJK row -> [UNK] by design, exercising unknown-token determinism).
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
    out = helper.make_tensor_value_info(
        "last_hidden_state", TensorProto.FLOAT, ["batch", "seq", DIM]
    )

    # deterministic pattern: table[v][d] = ((v * 8 + d) % 16) / 8 - 1  in [-1, 1)
    table = np.empty((VOCAB_SIZE, DIM), dtype=np.float32)
    for v in range(VOCAB_SIZE):
        for d in range(DIM):
            table[v][d] = ((v * 8 + d) % 16) / 8.0 - 1.0
    table_init = numpy_helper.from_array(table, name="embedding_table")

    gather = helper.make_node(
        "Gather", ["embedding_table", "input_ids"], ["last_hidden_state"], axis=0
    )

    graph = helper.make_graph(
        [gather],
        "tiny-embedder",
        [ids, mask],
        [out],
        initializer=[table_init],
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="vectorfs fixture generator",
    )
    # onnx 1.23 writes IR version 14 by default; ONNX Runtime 1.31 (the
    # pinned runtime) accepts at most IR 13. The graph itself uses one
    # ancient op — pin the IR version explicitly so the fixture loads.
    model.ir_version = 9
    onnx.checker.check_model(model)
    return model


def build_flat_model() -> onnx.ModelProto:
    """Gather -> Reshape(-1): a rank-1 output whose only dim is genuinely
    runtime-dependent (batch*seq*DIM). ONNX shape inference must leave it
    symbolic, so this fixture exercises the ort kind's "dim cannot be
    probed" startup failure (§5.10.2 contract) against the real runtime."""
    ids = helper.make_tensor_value_info(
        "input_ids", TensorProto.INT64, ["batch", "seq"]
    )
    mask = helper.make_tensor_value_info(
        "attention_mask", TensorProto.INT64, ["batch", "seq"]
    )
    out = helper.make_tensor_value_info(
        "last_hidden_state", TensorProto.FLOAT, ["flat"]
    )

    table = np.empty((VOCAB_SIZE, DIM), dtype=np.float32)
    for v in range(VOCAB_SIZE):
        for d in range(DIM):
            table[v][d] = ((v * 8 + d) % 16) / 8.0 - 1.0
    table_init = numpy_helper.from_array(table, name="embedding_table")
    shape_init = numpy_helper.from_array(np.array([-1], dtype=np.int64), name="flat_shape")

    gather = helper.make_node(
        "Gather", ["embedding_table", "input_ids"], ["gathered"], axis=0
    )
    reshape = helper.make_node("Reshape", ["gathered", "flat_shape"], ["last_hidden_state"])

    graph = helper.make_graph(
        [gather, reshape],
        "tiny-embedder-flat",
        [ids, mask],
        [out],
        initializer=[table_init, shape_init],
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="vectorfs fixture generator",
    )
    model.ir_version = 9
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

    model = build_model()
    onnx.save(model, out_dir / "model.onnx")
    onnx.save(build_flat_model(), out_dir / "model_flat.onnx")

    with open(out_dir / "tokenizer.json", "w", encoding="utf-8") as f:
        json.dump(build_tokenizer(), f, indent=2, ensure_ascii=False)
        f.write("\n")

    with open(out_dir / "config.json", "w", encoding="utf-8") as f:
        json.dump(
            {
                "model_type": "tiny-fixture",
                "hidden_size": DIM,
                "vocab_size": VOCAB_SIZE,
                "license": "CC0-1.0 (vectorfs test fixture, generated in-repo)",
            },
            f,
            indent=2,
        )
        f.write("\n")

    print(f"wrote {out_dir}/model.onnx model_flat.onnx tokenizer.json config.json")


if __name__ == "__main__":
    main()
