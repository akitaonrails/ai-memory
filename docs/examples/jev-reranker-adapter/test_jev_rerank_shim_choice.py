#!/usr/bin/env python3
"""Regression tests for jev_rerank_shim_choice probability validation.

Run from this directory:  python3 -m unittest test_jev_rerank_shim_choice -v
Stdlib only, no Jev backend required (HTTP is monkeypatched).
"""
import http.client
import io
import json
import threading
import unittest
import unittest.mock
from http.server import ThreadingHTTPServer

import jev_rerank_shim_choice as shim


def jev_response(probs):
    return {"answers": {"best": {"probabilities": probs}}}


def rerank_payload(ids=(1, 2, 3)):
    cands = [{"candidate": n, "title": f"page {n}", "text": f"text {n}"}
             for n in ids]
    return {"messages": [
        {"role": "system", "content": shim.RERANK_SYSTEM_PREFIX + " ..."},
        {"role": "user",
         "content": json.dumps({"query": "q", "candidates": cands})},
    ]}


class FakeResp(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


class ExtractChoiceProbsTest(unittest.TestCase):
    IDS = [1, 2, 3]

    def test_valid(self):
        vals = shim.extract_choice_probs(
            jev_response({"c1": 0.7, "c2": 0.2, "c3": 0.1}), self.IDS)
        self.assertEqual(vals, [0.7, 0.2, 0.1])

    def test_rounding_drift_tolerated(self):
        vals = shim.extract_choice_probs(
            jev_response({"c1": 0.3333, "c2": 0.3333, "c3": 0.3333}), self.IDS)
        self.assertAlmostEqual(sum(vals), 0.9999)

    def test_missing_probabilities_object(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs({"answers": {"best": {}}}, self.IDS)

    def test_non_object_response_rejected(self):
        for bad in ([], "c1", 0.5, None):
            with self.assertRaisesRegex(ValueError, "not a JSON object"):
                shim.extract_choice_probs(bad, self.IDS)

    def test_missing_answers(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs({}, self.IDS)

    def test_missing_key(self):
        with self.assertRaisesRegex(ValueError, "missing=.*c3"):
            shim.extract_choice_probs(
                jev_response({"c1": 0.6, "c2": 0.4}), self.IDS)

    def test_extra_key(self):
        with self.assertRaisesRegex(ValueError, "extra=.*c9"):
            shim.extract_choice_probs(
                jev_response({"c1": 0.5, "c2": 0.3, "c3": 0.2, "c9": 0.0}),
                self.IDS)

    def test_bool_rejected(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs(
                jev_response({"c1": True, "c2": 0.0, "c3": 1.0}), self.IDS)

    def test_string_rejected(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs(
                jev_response({"c1": "0.7", "c2": 0.2, "c3": 0.1}), self.IDS)

    def test_nan_rejected(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs(
                jev_response({"c1": float("nan"), "c2": 0.5, "c3": 0.5}),
                self.IDS)

    def test_inf_rejected(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs(
                jev_response({"c1": float("inf"), "c2": 0.0, "c3": 0.0}),
                self.IDS)

    def test_out_of_range_rejected(self):
        for bad in (1.7, -0.2):
            with self.assertRaises(ValueError):
                shim.extract_choice_probs(
                    jev_response({"c1": bad, "c2": 0.0, "c3": 0.0}),
                    self.IDS)

    def test_degenerate_sum_rejected(self):
        with self.assertRaisesRegex(ValueError, "sum"):
            shim.extract_choice_probs(
                jev_response({"c1": 0.9, "c2": 0.9, "c3": 0.9}), self.IDS)
        with self.assertRaisesRegex(ValueError, "sum"):
            shim.extract_choice_probs(
                jev_response({"c1": 0.0, "c2": 0.0, "c3": 0.0}), self.IDS)

    def test_duplicate_candidate_ids_rejected(self):
        with self.assertRaisesRegex(ValueError, "duplicate candidate"):
            shim.extract_choice_probs(
                jev_response({"c1": 0.6, "c2": 0.4}), [1, 1])

    def test_empty_candidates_rejected(self):
        with self.assertRaises(ValueError):
            shim.extract_choice_probs(jev_response({}), [])


class DuplicateJsonKeyTest(unittest.TestCase):
    def test_duplicate_keys_rejected(self):
        with self.assertRaises(ValueError):
            json.loads('{"c1": 0.6, "c1": 0.4}',
                       object_pairs_hook=shim._reject_dupes)

    def test_normal_json_accepted(self):
        d = json.loads('{"c1": 0.6, "c2": 0.4}',
                       object_pairs_hook=shim._reject_dupes)
        self.assertEqual(d, {"c1": 0.6, "c2": 0.4})


class JevRerankEndToEndTest(unittest.TestCase):
    def run_shim(self, response_json):
        return self.run_shim_raw(json.dumps(response_json).encode())

    def run_shim_raw(self, raw):
        with unittest.mock.patch.object(shim.urllib.request, "urlopen",
                                        return_value=FakeResp(raw)):
            return shim.jev_rerank(rerank_payload())

    def test_duplicate_json_key_in_response_raises(self):
        # Last-wins parsing would read c1=0.1 and accept a valid-looking
        # distribution; the shim must refuse the ambiguous payload instead.
        raw = (b'{"answers": {"best": {"probabilities": '
               b'{"c1": 0.9, "c1": 0.1, "c2": 0.5, "c3": 0.4}}}}')
        with self.assertRaisesRegex(ValueError, "duplicate key"):
            self.run_shim_raw(raw)

    def test_valid_response_maps_scores(self):
        out = self.run_shim(jev_response({"c1": 0.1, "c2": 0.7, "c3": 0.2}))
        self.assertEqual(out, {"scores": [
            {"candidate": 1, "relevance": 0.1},
            {"candidate": 2, "relevance": 0.7},
            {"candidate": 3, "relevance": 0.2},
        ]})

    def test_malformed_response_raises(self):
        # Old behaviour silently fabricated 0.0 for missing keys; now the
        # exception propagates so the handler answers 500 and ai-memory
        # keeps its original order.
        with self.assertRaises(ValueError):
            self.run_shim(jev_response({"c1": 0.9}))

    def test_200_with_wrong_shape_raises(self):
        with self.assertRaises(ValueError):
            self.run_shim({"answers": {"best": {"choice": "c1"}}})


class HandlerFailClosedTest(unittest.TestCase):
    """The documented contract: a malformed judge answer becomes HTTP 500."""

    def post_rerank(self, jev_json):
        srv = ThreadingHTTPServer(("127.0.0.1", 0), shim.Handler)
        thread = threading.Thread(
            target=srv.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        fake = FakeResp(json.dumps(jev_json).encode())
        conn = http.client.HTTPConnection(*srv.server_address, timeout=5)
        self.addCleanup(conn.close)
        with unittest.mock.patch.object(shim.urllib.request, "urlopen",
                                        return_value=fake), \
                unittest.mock.patch.object(shim, "log"):
            conn.request("POST", "/v1/chat/completions",
                         body=json.dumps(rerank_payload()),
                         headers={"Content-Type": "application/json"})
            resp = conn.getresponse()
            return resp.status, resp.read()

    def test_malformed_probabilities_answer_500(self):
        status, _ = self.post_rerank(jev_response({"c1": 0.9}))
        self.assertEqual(status, 500)

    def test_valid_probabilities_answer_200_with_scores(self):
        status, body = self.post_rerank(
            jev_response({"c1": 0.1, "c2": 0.7, "c3": 0.2}))
        self.assertEqual(status, 200)
        content = json.loads(body)["choices"][0]["message"]["content"]
        self.assertEqual(json.loads(content)["scores"][1],
                         {"candidate": 2, "relevance": 0.7})


if __name__ == "__main__":
    unittest.main()
