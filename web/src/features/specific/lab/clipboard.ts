// Browsers hide the clipboard API on plain HTTP, which a tailnet address is.
export async function writeClipboard(text: string) {
  if (navigator.clipboard) return navigator.clipboard.writeText(text)
  const box = document.createElement("textarea")
  box.value = text
  document.body.append(box)
  box.select()
  const copied = document.execCommand("copy")
  box.remove()
  if (!copied) throw new Error("clipboard unavailable")
}
