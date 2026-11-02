// Небольшие улучшения поверх обычных форм: подтверждение опасных действий и копирование.
document.addEventListener("submit", (event) => {
  const form = event.target;
  const button = event.submitter;
  const message = (button && button.dataset.confirmButton) || form.dataset.confirm;
  if (message && !window.confirm(message)) {
    event.preventDefault();
  }
});

document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy]");
  if (!button) return;
  const source = document.getElementById(button.dataset.copy);
  if (!source) return;
  const text = source.innerText.trim();
  const label = button.textContent;
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "Скопировано";
  } catch {
    const range = document.createRange();
    range.selectNodeContents(source);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    button.textContent = "Выделено — нажмите Ctrl+C";
  }
  setTimeout(() => { button.textContent = label; }, 2000);
});
