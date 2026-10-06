// Tailwind for the Clay & Kiln example shop.
// Rebuild after editing a template:
//   npx tailwindcss@3 -c examples/ceramics_shop/tailwind.config.js \
//     -i examples/ceramics_shop/shop.src.css -o examples/ceramics_shop/static/shop.css --minify
module.exports = {
  content: [__dirname + "/templates/**/*.html"],
  theme: {
    extend: {
      colors: {
        clay: { 50: "#fbf6f1", 100: "#f4e9dd", 200: "#e8d2bb", 300: "#d8b392", 400: "#c48d65", 500: "#b5693f", 600: "#9c5431", 700: "#7e4229", 800: "#5f3322", 900: "#3f2318" },
        sand: "#faf6f0",
        ink: "#262320",
      },
      fontFamily: {
        display: ['"Fraunces"', "Georgia", "serif"],
        sans: ['"Inter"', "system-ui", "sans-serif"],
      },
    },
  },
  plugins: [],
};
