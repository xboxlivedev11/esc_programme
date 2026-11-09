// Runs before the first paint so a chosen theme never flashes the wrong colours.
try {
  var p = JSON.parse(localStorage.getItem('esc-prefs') || '{}');
  if (p.theme === 'light' || p.theme === 'dark') document.documentElement.setAttribute('data-theme', p.theme);
} catch (e) {}
