export function keepLegacyFallback(response) {
  return response.items || [];
}
