// Synthetic labels for requirement discovery.
export const labels = { en: { facebook: "Continue with Facebook" }, ar: { facebook: "المتابعة باستخدام فيسبوك" } };
export function visibleProviders(providers: { enabled: boolean }[]) {
  return providers.filter(provider => provider.enabled);
}
