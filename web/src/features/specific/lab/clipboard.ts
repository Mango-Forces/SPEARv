/** Copy text on HTTPS and on plain HTTP (Tailscale). `navigator.clipboard`
 * often exists on HTTP but rejects writeText outside a secure context. */
export async function writeClipboard(text: string) {
  if (typeof window !== "undefined" && window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text)
      return
    } catch {
      // Fall through.
    }
  }
  const box = document.createElement("textarea")
  box.value = text
  box.style.position = "fixed"
  box.style.left = "0"
  box.style.top = "0"
  box.style.opacity = "0"
  box.setAttribute("readonly", "")
  document.body.append(box)
  box.focus()
  box.select()
  box.setSelectionRange(0, text.length)
  let copied = false
  try {
    copied = document.execCommand("copy")
  } finally {
    box.remove()
  }
  if (!copied) throw new Error("clipboard unavailable")
}
