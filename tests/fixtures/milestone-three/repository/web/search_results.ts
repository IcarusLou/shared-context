export interface SearchResponseV2 {
  items: string[];
}

export type SearchEnvelopeClient = SearchResponseV2;

export function openSearchResults(response: SearchResponseV2) {
  return response.items.length;
}

export async function loadSearchResults() {
  return fetch("/v2/search/client");
}
