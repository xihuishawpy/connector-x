import os

import pandas as pd
import pytest
from pandas.testing import assert_frame_equal

from .. import partition_sql


@pytest.fixture(scope="module")  # type: ignore
def postgres_url() -> str:
    return os.environ["POSTGRES_URL"]


def test_partition_sql(postgres_url: str) -> None:
    query = "SELECT * FROM test_table"
    queires = partition_sql(
        postgres_url, query, partition_on="test_int", partition_num=2
    )
    assert len(queires) == 2
