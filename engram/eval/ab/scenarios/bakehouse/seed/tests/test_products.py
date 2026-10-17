import os
import unittest

from bakehouse.products import Catalog, Product, load_catalog

DATA = os.path.join(os.path.dirname(__file__), "..", "data")


class CatalogTest(unittest.TestCase):
    def test_add_and_get(self):
        catalog = Catalog([Product("SOU", "White sourdough", 320, batch_size=10, dough_g=800, bake_temp=240)])
        self.assertIn("SOU", catalog)
        self.assertEqual(catalog.get("SOU").price, 320)

    def test_duplicate_code_rejected(self):
        catalog = Catalog([Product("SOU", "White sourdough", 320)])
        with self.assertRaises(ValueError):
            catalog.add(Product("SOU", "Something else", 100))

    def test_load_catalog(self):
        catalog = load_catalog(os.path.join(DATA, "products.csv"))
        self.assertEqual(catalog.get("RYE").allergens, ("rye", "wheat", "sesame"))
        self.assertEqual(catalog.get("BAG").batch_size, 12)
        self.assertEqual([p.code for p in catalog][:3], ["BAG", "BKW", "BRO"])


if __name__ == "__main__":
    unittest.main()
