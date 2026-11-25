const workflow = document.querySelector('#workflow video');

document.querySelector('.watch')?.addEventListener('click', () => {
  workflow?.play().catch(() => {
    // Native controls remain available when the browser blocks playback.
  });
});
