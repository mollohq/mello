"""Unit tests for the art index export. No network: python3 -m unittest."""

import csv
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import build_catalogue as bc  # noqa: E402


def write(dir_, name, header, rows):
    with open(os.path.join(dir_, f"{name}.csv"), "w", encoding="utf-8", newline="") as f:
        w = csv.writer(f, lineterminator="\n")
        w.writerow(header)
        w.writerows(rows)


def read(dir_, name):
    with open(os.path.join(dir_, f"{name}.csv"), encoding="utf-8", newline="") as f:
        return list(csv.reader(f))


class ArtExport(unittest.TestCase):
    def fixture(self, tmp):
        write(tmp, "games", ["id", "name", "slug", "platforms", "follows"],
              [["1", "Cover, And Shot", "cs", "{6}", "3"], ["2", "Two\nLines", "tl", "{14}", "0"]])
        write(tmp, "covers", ["id", "game", "image_id", "width", "height", "url"], [["10", "1", "co1", "528", "748", "x"]])
        write(tmp, "artworks", ["id", "game", "image_id", "width", "height"], [["20", "1", "ar1", "1920", "1080"]])
        write(tmp, "screenshots", ["id", "game", "image_id", "width", "height"], [["30", "2", "sc2", "1280", "720"]])
        write(tmp, "external_games", ["id", "game", "uid", "external_game_source", "year"], [["40", "1", "440", "1", "2020"]])
        write(tmp, "external_game_sources", ["id", "name"], [["1", "Steam"]])

    def test_export_keeps_names_columns_and_quoting(self):
        with tempfile.TemporaryDirectory() as src, tempfile.TemporaryDirectory() as out:
            self.fixture(src)
            counts = bc.export_art_index(src, os.path.join(out, "dumps"))
            self.assertEqual(counts["games"], 2)
            self.assertEqual(counts["artworks"], 1)
            d = os.path.join(out, "dumps")
            self.assertEqual(read(d, "games")[0], ["id", "name", "slug"])
            self.assertEqual(read(d, "games")[1], ["1", "Cover, And Shot", "cs"])
            self.assertEqual(read(d, "games")[2], ["2", "Two\nLines", "tl"])
            self.assertEqual(read(d, "covers")[1], ["10", "1", "co1", "528", "748"])
            self.assertEqual(read(d, "external_games")[1], ["40", "1", "440", "1"])
            self.assertEqual(read(d, "screenshots")[1][2], "sc2")

    def test_missing_column_stops(self):
        with tempfile.TemporaryDirectory() as src, tempfile.TemporaryDirectory() as out:
            self.fixture(src)
            write(src, "artworks", ["id", "game"], [["20", "1"]])
            with self.assertRaises(SystemExit):
                bc.export_art_index(src, out)

    def test_endpoints_include_art_dumps(self):
        self.assertIn("artworks", bc.ENDPOINTS)
        self.assertIn("screenshots", bc.ENDPOINTS)


if __name__ == "__main__":
    unittest.main()
