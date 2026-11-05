import "./strips.css";
import { boot } from "./app";

void boot(document, window).catch(error => console.error(error));
