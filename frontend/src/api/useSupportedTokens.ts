import { useQuery } from "@tanstack/react-query";

import { listTokens, type TokenDto } from "./tokenAdmin";

/** Live view of token-info's full catalog (enabled + disabled). */
export function useSupportedTokens() {
  return useQuery<TokenDto[], Error>({
    queryKey: ["supported-tokens"],
    queryFn: listTokens,
  });
}
