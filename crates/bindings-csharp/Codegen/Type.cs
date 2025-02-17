namespace SpacetimeDB.Codegen;

using System;
using System.Collections.Generic;
using System.Collections.Immutable;
using System.Linq;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using static Utils;

struct VariableDeclaration(string name, ITypeSymbol typeSymbol)
{
    public string Name = name;
    public string Type = SymbolToName(typeSymbol);
    public string TypeInfo = GetTypeInfo(typeSymbol);
}

[Generator]
public class Type : IIncrementalGenerator
{
    public void Initialize(IncrementalGeneratorInitializationContext context)
    {
        WithAttrAndPredicate(
            context,
            "SpacetimeDB.TypeAttribute",
            (node) =>
            {
                // structs and classes should be always processed
                if (node is not EnumDeclarationSyntax enumType)
                    return true;

                // Ensure variants are contiguous as SATS enums don't support explicit tags.
                if (enumType.Members.Any(m => m.EqualsValue is not null))
                {
                    throw new InvalidOperationException(
                        "[SpacetimeDB.Type] enums cannot have explicit values: "
                            + enumType.Identifier
                    );
                }

                // Ensure all enums fit in `byte` as that's what SATS uses for tags.
                if (enumType.Members.Count > 256)
                {
                    throw new InvalidOperationException(
                        "[SpacetimeDB.Type] enums cannot have more than 256 variants."
                    );
                }

                // Check that enums are compatible with SATS but otherwise skip from extra processing.
                return false;
            }
        );

        // Any table should be treated as a type without an explicit [Type] attribute.
        WithAttrAndPredicate(context, "SpacetimeDB.TableAttribute", (_node) => true);
    }

    public static void WithAttrAndPredicate(
        IncrementalGeneratorInitializationContext context,
        string fullyQualifiedMetadataName,
        Func<SyntaxNode, bool> predicate
    )
    {
        context
            .SyntaxProvider.ForAttributeWithMetadataName(
                fullyQualifiedMetadataName,
                predicate: (node, ct) => predicate(node),
                transform: (context, ct) =>
                {
                    var type = (TypeDeclarationSyntax)context.TargetNode;

                    // Check if type implements generic `SpacetimeDB.TaggedEnum<Variants>` and, if so, extract the `Variants` type.
                    var taggedEnumVariants = type.BaseList?.Types
                        .OfType<SimpleBaseTypeSyntax>()
                        .Select(t => context.SemanticModel.GetTypeInfo(t.Type, ct).Type)
                        .OfType<INamedTypeSymbol>()
                        .Where(t =>
                            t.OriginalDefinition.ToString() == "SpacetimeDB.TaggedEnum<Variants>"
                        )
                        .Select(t =>
                            (ImmutableArray<IFieldSymbol>?)
                                ((INamedTypeSymbol)t.TypeArguments[0]).TupleElements
                        )
                        .FirstOrDefault();

                    var fields = type.Members.OfType<FieldDeclarationSyntax>()
                        .Where(f =>
                            !f.Modifiers.Any(m =>
                                m.IsKind(SyntaxKind.StaticKeyword)
                                || m.IsKind(SyntaxKind.ConstKeyword)
                            )
                        )
                        .SelectMany(f =>
                        {
                            var typeSymbol = context
                                .SemanticModel.GetTypeInfo(f.Declaration.Type, ct)
                                .Type!;
                            // Seems like a bug in Roslyn - nullability annotation is not set on the top type.
                            // Set it manually for now. TODO: report upstream.
                            if (f.Declaration.Type is NullableTypeSyntax)
                            {
                                typeSymbol = typeSymbol.WithNullableAnnotation(
                                    NullableAnnotation.Annotated
                                );
                            }
                            return f.Declaration.Variables.Select(v => new VariableDeclaration(
                                v.Identifier.Text,
                                typeSymbol
                            ));
                        });

                    if (taggedEnumVariants is not null)
                    {
                        if (fields.Any())
                        {
                            throw new InvalidOperationException("Tagged enums cannot have fields.");
                        }
                        fields = taggedEnumVariants.Value.Select(v => new VariableDeclaration(
                            v.Name,
                            v.Type
                        ));
                    }

                    return new
                    {
                        Scope = new Scope(type),
                        ShortName = type.Identifier.Text,
                        FullName = SymbolToName(context.SemanticModel.GetDeclaredSymbol(type, ct)!),
                        GenericName = $"{type.Identifier}{type.TypeParameterList}",
                        IsTaggedEnum = taggedEnumVariants is not null,
                        TypeParams = type.TypeParameterList?.Parameters
                            .Select(p => p.Identifier.Text)
                            .ToArray() ?? [],
                        Members = fields.ToArray(),
                    };
                }
            )
            .Select(
                (type, ct) =>
                {
                    string typeKind,
                        read,
                        write;

                    var typeDesc = "";

                    var fieldNames = type.Members.Select(m => m.Name);

                    if (type.IsTaggedEnum)
                    {
                        typeKind = "Sum";

                        typeDesc +=
                            $@"
                            private {type.ShortName}() {{ }}

                            public enum @enum: byte
                            {{
                                {string.Join(",\n", fieldNames)}
                            }}
                        ";

                        typeDesc += string.Join(
                            "\n",
                            type.Members.Select(m =>
                                // C# puts field names in the same namespace as records themselves, and will complain about clashes if they match.
                                // To avoid this, we append an underscore to the field name.
                                // In most cases the field name shouldn't matter anyway as you'll idiomatically use pattern matching to extract the value.
                                $@"public sealed record {m.Name}({m.Type} {m.Name}_) : {type.ShortName};"
                            )
                        );

                        read =
                            $@"(@enum)reader.ReadByte() switch {{
                                {string.Join("\n", fieldNames.Select(name => $"@enum.{name} => new {name}(fieldTypeInfo.{name}.Read(reader)),"))}
                                _ => throw new System.InvalidOperationException(""Invalid tag value, this state should be unreachable."")
                            }}";

                        write =
                            $@"switch (value) {{
                                {string.Join("\n", fieldNames.Select(name => $@"
                                    case {name}(var inner):
                                        writer.Write((byte)@enum.{name});
                                        fieldTypeInfo.{name}.Write(writer, inner);
                                        break;
                                "))}
                            }}";
                    }
                    else
                    {
                        typeKind = "Product";

                        read =
                            $@"new {type.GenericName} {{
                                {string.Join(",\n", fieldNames.Select(name => $"{name} = fieldTypeInfo.{name}.Read(reader)"))}
                            }}";

                        write = string.Join(
                            "\n",
                            fieldNames.Select(name =>
                                $"fieldTypeInfo.{name}.Write(writer, value.{name});"
                            )
                        );
                    }

                    typeDesc +=
                        $@"
private static SpacetimeDB.SATS.TypeInfo<{type.GenericName}>? satsTypeInfo;

public static SpacetimeDB.SATS.TypeInfo<{type.GenericName}> GetSatsTypeInfo({string.Join(", ", type.TypeParams.Select(p => $"SpacetimeDB.SATS.TypeInfo<{p}> {p}TypeInfo"))}) {{
    if (satsTypeInfo is not null) {{
        return satsTypeInfo;
    }}
    var typeRef = SpacetimeDB.Module.FFI.AllocTypeRef();
    // Careful with the order: to prevent infinite recursion, we need to assign satsTypeInfo first,
    // and populate fieldTypeInfo and, correspondingly, read/write implementations, after that.
    System.Func<System.IO.BinaryReader, {type.GenericName}> read = (reader) => throw new System.InvalidOperationException(""Recursive type is not yet initialized"");
    System.Action<System.IO.BinaryWriter, {type.GenericName}> write = (writer, value) => throw new System.InvalidOperationException(""Recursive type is not yet initialized"");
    satsTypeInfo = new(
        typeRef,
        (reader) => read(reader),
        (writer, value) => write(writer, value)
    );
    var fieldTypeInfo = new {{
        {string.Join("\n", type.Members.Select(m => $"{m.Name} = {m.TypeInfo},"))}
    }};
    SpacetimeDB.Module.FFI.SetTypeRef<{type.GenericName}>(
        typeRef,
        new SpacetimeDB.SATS.AlgebraicType.{typeKind}(new SpacetimeDB.SATS.AggregateElement[] {{
            {string.Join("\n", type.Members.Select(m => $"new(nameof({m.Name}), fieldTypeInfo.{m.Name}.AlgebraicType),"))}
        }}),
        {(
            fullyQualifiedMetadataName == "SpacetimeDB.TableAttribute"
            // anonymous (don't register type alias) if it's a table that will register its own name in a different way
            ? "true"
            : "false"
        )}
    );
    read = (reader) => {read};
    write = (writer, value) => {{
        {write}
    }};
    return satsTypeInfo;
}}
                    ";

                    return new KeyValuePair<string, string>(
                        type.FullName,
                        type.Scope.GenerateExtensions(typeDesc)
                    );
                }
            )
            .RegisterSourceOutputs(context);
    }
}
