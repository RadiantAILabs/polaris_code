import unittest

from app import add, greeting


class AppTest(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_greeting(self):
        self.assertEqual(greeting("world"), "hello, world")


if __name__ == "__main__":
    unittest.main()
