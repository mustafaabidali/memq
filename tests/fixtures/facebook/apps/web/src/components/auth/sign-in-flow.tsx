// Synthetic call boundary. No network or provider credentials.
export function signInWithOAuth(input: { provider: string, locale: string }) {
  return { fixtureOnly: true, ...input };
}
export function beginSignIn(provider: string, locale: string) {
  return signInWithOAuth({ provider, locale });
}
