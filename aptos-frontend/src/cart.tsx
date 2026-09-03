import { useState } from "react";
import type { Listing } from "./api";

const KEY = "nft-cart-v1";

export interface CartItem extends Listing {
  venue: number;
  standard: string;
}

function load(): CartItem[] {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? "[]") as CartItem[];
  } catch {
    return [];
  }
}

export function useCart() {
  const [items, setItems] = useState<CartItem[]>(load);
  const save = (next: CartItem[]) => {
    setItems(next);
    localStorage.setItem(KEY, JSON.stringify(next));
  };
  return {
    items,
    add: (item: CartItem) => {
      if (!items.some((i) => i.listing_id === item.listing_id)) {
        save([...items, item]);
      }
    },
    remove: (listingId: string) =>
      save(items.filter((i) => i.listing_id !== listingId)),
    clear: () => save([]),
  };
}
