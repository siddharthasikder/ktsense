package shop.repeated

public value class TypeOfService(/* 𝕊𝕊𝕊𝕊 */ public val value: UByte)

// Line 3 above carries `value` twice: once as Kotlin's soft keyword at character column 8, once as
// the declared property at character column 56. The four astral-plane characters between them put
// the property's byte column at 68 and its UTF-16 column at 60, so a locator counting the wrong unit
// cannot agree with this file by accident. Shape taken from ktor's TypeOfService, whose engine-
// reported column is the property's. Every note is kept below the line so it cannot shift it.
