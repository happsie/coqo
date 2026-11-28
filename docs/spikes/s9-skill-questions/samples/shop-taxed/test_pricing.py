import pytest

from shop.pricing import apply_discount, total


def test_volume_discount():
    assert apply_discount(10.0, 1) == 10.0
    assert apply_discount(10.0, 10) == 9.5
    assert apply_discount(10.0, 100) == 9.0


def test_total_adds_shipping():
    assert total([(10.0, 1)]) == 14.9


def test_total_adds_tax_after_shipping():
    assert total([(10.0, 1)], tax_rate=0.25) == 18.62


def test_negative_tax_rate_is_refused():
    with pytest.raises(ValueError):
        total([(10.0, 1)], tax_rate=-0.1)
