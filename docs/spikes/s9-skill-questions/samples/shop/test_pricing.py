from shop.pricing import apply_discount, total


def test_volume_discount():
    assert apply_discount(10.0, 1) == 10.0
    assert apply_discount(10.0, 10) == 9.5
    assert apply_discount(10.0, 100) == 9.0


def test_total_adds_shipping():
    assert total([(10.0, 1)]) == 14.9
