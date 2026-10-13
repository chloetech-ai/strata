import unittest

from stockroom.items import Catalog, Item


class CatalogTest(unittest.TestCase):
    def test_add_and_get(self):
        catalog = Catalog([Item("BR-1042", "Hex bolt M8x40", "C-03-2", reorder_point=50, case_size=100)])
        self.assertIn("BR-1042", catalog)
        self.assertEqual(catalog.get("BR-1042").name, "Hex bolt M8x40")

    def test_duplicate_sku_rejected(self):
        catalog = Catalog([Item("BR-1042", "Hex bolt M8x40", "C-03-2")])
        with self.assertRaises(ValueError):
            catalog.add(Item("BR-1042", "Something else", "A-01-1"))

    def test_iterates_by_sku(self):
        catalog = Catalog([Item("BR-2", "Nut", "A-01-1"), Item("BR-1", "Bolt", "B-01-1")])
        self.assertEqual([item.sku for item in catalog], ["BR-1", "BR-2"])


if __name__ == "__main__":
    unittest.main()
