"""Scraper worker entrypoint: `python -m app.worker`."""

import logging

from app.scheduler import run_forever

if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    run_forever()
