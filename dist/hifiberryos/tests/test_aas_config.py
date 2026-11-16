import importlib.machinery
import importlib.util
import json
import os
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
loader = importlib.machinery.SourceFileLoader(
    "aas_config", os.path.join(HERE, "..", "aas-config"))
spec = importlib.util.spec_from_loader("aas_config", loader)
mod = importlib.util.module_from_spec(spec)
loader.exec_module(mod)

FIXTURES = ["dacplus-adc-pro", "digiplus", "amp-no-control", "no-card"]


def fixture_path(name):
    return os.path.join(HERE, "systeminfo", name + ".json")


def load(name):
    with open(fixture_path(name)) as f:
        return json.load(f)


class ConfigFrom(unittest.TestCase):
    def test_dacplus_adc_pro(self):
        c = mod.config_from(load("dacplus-adc-pro"))
        self.assertEqual(c["card"], "0")
        self.assertEqual(c["volume_control"], "Digital")
        self.assertNotIn("volume_mode", c)
        self.assertIs(c["analog"]["enabled"], True)
        self.assertEqual(c["plugin"],
                         [{"place": "output", "type": "balance", "balance": 0}])

    def test_digi(self):
        c = mod.config_from(load("digiplus"))
        self.assertNotIn("analog", c)
        self.assertEqual(c["volume_control"], "Softvol")
        self.assertEqual(c["volume_mode"], "follow")

    def test_digi_as_string(self):
        info = load("digiplus")
        info["soundcard"]["card_type"] = "Digi"
        self.assertNotIn("analog", mod.config_from(info))

    def test_amp(self):
        c = mod.config_from(load("amp-no-control"))
        self.assertEqual(c["volume_control"], "Softvol")
        self.assertEqual(c["volume_mode"], "follow")
        self.assertNotIn("analog", c)

    def test_no_card(self):
        c = mod.config_from(load("no-card"))
        self.assertNotIn("card", c)
        self.assertNotIn("volume_control", c)
        self.assertNotIn("volume_mode", c)


class Merge(unittest.TestCase):
    def setUp(self):
        self.gen = mod.config_from(load("dacplus-adc-pro"))

    def test_added(self):
        self.assertEqual(mod.merge(self.gen, {"latency_ms": 8})["latency_ms"], 8)

    def test_local_wins(self):
        self.assertEqual(mod.merge(self.gen, {"card": "1"})["card"], "1")

    def test_table_merged(self):
        m = mod.merge(self.gen, {"analog": {"volume_db": -6}})
        self.assertEqual(m["analog"], {"enabled": True, "volume_db": -6})

    def test_array_replaced(self):
        local = {"plugin": [{"place": "mix", "type": "balance", "balance": 1}]}
        self.assertEqual(mod.merge(self.gen, local)["plugin"], local["plugin"])


class Dump(unittest.TestCase):
    def test_round_trip(self):
        for n in FIXTURES:
            c = mod.config_from(load(n))
            text = mod.dump(c)
            self.assertEqual(tomllib.loads(text), c, n)
            self.assertTrue(text.startswith("# Generated at every boot"))

    def test_merged_and_escapes(self):
        c = mod.merge(mod.config_from(load("dacplus-adc-pro")),
                      {"latency_ms": 7.5, "note": 'a "q"\\ \n',
                       "analog": {"volume_db": -6},
                       "matrix": [{"from": "players.left", "to": 1}]})
        text = mod.dump(c)
        self.assertEqual(tomllib.loads(text), c)
        self.assertTrue(text.startswith(mod.HEADER))


class Main(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out = os.path.join(self.tmp.name, "aas.toml")
        self.local = os.path.join(self.tmp.name, "aas.local.toml")
        self.missing = os.path.join(self.tmp.name, "nope.json")

    def run_main(self, info):
        self.assertEqual(mod.main(["--systeminfo", info, "--output", self.out,
                                   "--local", self.local]), 0)

    def read(self):
        with open(self.out) as f:
            return f.read()

    def test_missing_keeps_existing(self):
        with open(self.out, "w") as f:
            f.write("latency_ms = 5\n")
        self.run_main(self.missing)
        self.assertEqual(self.read(), "latency_ms = 5\n")

    def test_missing_without_output(self):
        self.run_main(self.missing)
        self.assertEqual(self.read(), mod.HEADER)

    def test_missing_without_output_merges_local(self):
        with open(self.local, "w") as f:
            f.write("latency_ms = 8\n")
        self.run_main(self.missing)
        self.assertEqual(tomllib.loads(self.read()), {"latency_ms": 8})

    def test_invalid_local_ignored(self):
        self.run_main(fixture_path("dacplus-adc-pro"))
        good = self.read()
        with open(self.local, "w") as f:
            f.write("this is [not toml\n")
        self.run_main(fixture_path("dacplus-adc-pro"))
        self.assertEqual(self.read(), good)

    def test_local_merged(self):
        with open(self.local, "w") as f:
            f.write("card = \"1\"\n[analog]\nvolume_db = -6\n")
        self.run_main(fixture_path("dacplus-adc-pro"))
        c = tomllib.loads(self.read())
        self.assertEqual(c["card"], "1")
        self.assertEqual(c["analog"], {"enabled": True, "volume_db": -6})

    def test_unwritable_local_value_dropped(self):
        with open(self.local, "w") as f:
            f.write("when = 2026-11-16T10:00:00\nlatency_ms = 8\n")
        self.run_main(fixture_path("dacplus-adc-pro"))
        self.assertEqual(tomllib.loads(self.read()),
                         mod.config_from(load("dacplus-adc-pro")))

    def test_failed_write_leaves_no_temp_file(self):
        os.mkdir(self.out)  # the rename onto a directory fails
        self.run_main(fixture_path("dacplus-adc-pro"))
        self.assertEqual(os.listdir(self.tmp.name), ["aas.toml"])


class Fetch(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def script(self, body):
        path = os.path.join(self.tmp.name, "config-soundcard")
        with open(path, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(path, 0o755)
        return [path, "--json"]

    def test_ok(self):
        cmd = self.script("echo '{\"hardware_index\": 2}'")
        self.assertEqual(mod.fetch(cmd), {"soundcard": {"hardware_index": 2}})

    def test_missing(self):
        self.assertIsNone(mod.fetch([os.path.join(self.tmp.name, "none")]))

    def test_failure(self):
        self.assertIsNone(mod.fetch(self.script("echo '{}'; exit 1")))

    def test_bad_json(self):
        self.assertIsNone(mod.fetch(self.script("echo nope")))

    def test_not_an_object(self):
        self.assertIsNone(mod.fetch(self.script("echo '[1]'")))

    def test_timeout(self):
        old = mod.TIMEOUT
        mod.TIMEOUT = 0.2
        self.addCleanup(setattr, mod, "TIMEOUT", old)
        self.assertIsNone(mod.fetch(self.script("sleep 5")))


if __name__ == "__main__":
    unittest.main()
